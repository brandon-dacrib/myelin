# Synapse configuration translation table

Track 13 (`docs/workstreams/13-config-compat-and-migration.md`). Source: every one of the 229 documented top-level options and 51 `experimental_features` flags listed in `docs/synapse-inventory.md` (generated from Synapse 1.161.0's `refs/synapse/docs/usage/configuration/config_documentation.md`). This is the analysis `PLAN.md` section 9.2 requires: every option has exactly one of three fates.

- **Mapped** — same behavior, same default, a named `hs-config` field.
- **Mapped (diff)** — a named `hs-config` field exists, but the shape, granularity or default differs; the note says how.
- **Unsupported** — no `hs-config` field. The translator (`hs-compat`) rejects a `homeserver.yaml` that sets it to a non-default value unless run with `--allow-unsupported-synapse-config`, and always includes it in the translation report. A reason code (below) plus, where useful, a specific note explains why.

Native field paths are dotted `hs-config` `Config` paths, e.g. `federation.domain_allowlist`, as defined in `crates/hs-config/src/*.rs`.

## Summary

| List | Mapped | Mapped (diff) | Unsupported | Total |
|---|---|---|---|---|
| Top-level options | 26 | 24 | 179 | 229 |
| `experimental_features` flags | 0 | 1 | 50 | 51 |

(Counts are exact against the tables below; regenerate this summary whenever a row changes. See "Keeping this current".)

## Reason codes (for Unsupported rows)

| Code | Meaning |
|---|---|
| **R-PROC** | Process supervision (daemonizing, PID files, a debug REPL) is left to the container/systemd/Kubernetes platform (`PLAN.md` D11, section 7); not a config concern for a process that runs in the foreground under a supervisor. |
| **R-WORKER** | Superseded by identical replicas with lease-based ownership (`PLAN.md` D2). Synapse's worker split, replication streams, `instance_map` and worker-specific listeners have no native equivalent; the importer reads worker configs only to collapse them into one process, never as a native config key (`PLAN.md` section 9.1). |
| **R-PY** | Python-runtime-specific tuning (cyclic garbage collector thresholds, frozen dicts) with no meaning in a Rust process. |
| **R-MAU** | Monthly-active-user hosting-capacity billing/blocking controls specific to Synapse's largest hosted deployments (matrix.org-scale). Not planned; Kubernetes-level resource quotas and the admin API's user-suspension tools cover the operational need. |
| **R-PHASE1** | Not in the Phase 0 native schema. The underlying feature is real and owned by another track (named in the note); it will grow its own config surface, likely under `hs-config`, in Phase 1 (`PLAN.md` section 11's workstream table). Listed here as of day one; re-check each Synapse release and each time the owning track ships. |
| **R-MODULE** | Python module hook. Superseded by `hs-modules`'s HTTP-callback and WebAssembly module points (`PLAN.md` D8, section 9.5), which will have their own config surface separate from `hs-config`. |
| **R-SSO-LEGACY** | A legacy or niche SSO protocol Synapse itself steers operators away from. Not planned; operators migrate to native OIDC or MAS delegation before cutover. |
| **R-SECURITY** | Deliberately not implemented because it weakens a security boundary this design does not want to offer (e.g. legacy query-string appservice tokens, certificate-verification bypass lists). |
| **R-NOFLAG** | (`experimental_features` list only.) The corresponding functionality, once implemented, ships unconditionally rather than behind an opt-in flag: there is no `experimental_features`-style gate here (`PLAN.md` D10, section 10.4, names the MSCs this server commits to). |
| **R-NOTPLANNED** | (`experimental_features` list only.) Not on the required MSC list in `PLAN.md` section 10.4. Tracked on demand if a bridge, client or the spec itself promotes it; not a gap in Phase 0. |

## Keeping this current

`tools/synapse_inventory.py` regenerates `docs/synapse-inventory.md`'s option list per Synapse release (track 13's pinned-version policy). When it changes: diff the option list against this file's row set, add new rows (default to Unsupported/R-PHASE1 pending review), remove rows for options Synapse itself dropped, and update the summary counts.

---

## Modules

| Option | Status | Native | Notes |
|---|---|---|---|
| `modules` | Unsupported | — | R-MODULE. |

## Server

| Option | Status | Native | Notes |
|---|---|---|---|
| `server_name` | Mapped | `server.server_name` | |
| `pid_file` | Unsupported | — | R-PROC. |
| `daemonize` | Unsupported | — | R-PROC. |
| `print_pidfile` | Unsupported | — | R-PROC. |
| `user_agent_suffix` | Unsupported | — | R-PHASE1 (hs-http). Cosmetic `User-Agent` string override; fixed native string for now. |
| `use_frozen_dicts` | Unsupported | — | R-PY. |
| `web_client_location` | Unsupported | — | Redirecting `/` to a bundled web client is not implemented; use a reverse-proxy redirect. |
| `public_baseurl` | Mapped | `server.public_baseurl` | |
| `serve_server_wellknown` | Mapped (diff) | `server.well_known_server` | Synapse takes a boolean and derives the advertised value from `server_name` itself (`host:443`, or `server_name` verbatim if it already names a port); the native field takes the advertised `host[:port]` directly instead. The translator reproduces Synapse's derivation exactly. Unset means the route 404s (`crates/hs-cli/src/well_known.rs`, checked). |
| `extra_well_known_client_content` | Unsupported | — | R-PHASE1. `/.well-known/matrix/client` is served from `server.public_baseurl` (`crates/hs-cli/src/well_known.rs`, checked) but carries only `m.homeserver`; arbitrary extra keys have no native field yet. |
| `soft_file_limit` | Unsupported | — | R-PROC. `ulimit` is a deployment-platform concern. |
| `presence` | Unsupported | — | R-PHASE1 (hs-user). Presence enable/tuning is owned by the user-session actor's own config, not yet exposed in `hs-config`. |
| `require_auth_for_profile_requests` | Unsupported | — | R-PHASE1 (hs-user/profile). |
| `limit_profile_requests_to_users_who_share_rooms` | Unsupported | — | R-PHASE1 (hs-user/profile). |
| `include_profile_data_on_invite` | Unsupported | — | R-PHASE1 (hs-user/profile). |
| `include_profile_updates_in_sync` | Unsupported | — | R-PHASE1 (hs-user). |
| `allow_public_rooms_without_auth` | Unsupported | — | R-PHASE1 (hs-room directory). |
| `allow_public_rooms_over_federation` | Mapped | `federation.allow_public_rooms_over_federation` | |
| `default_room_version` | Unsupported | — | R-PHASE1 (hs-state/hs-room). Room-version default selection is not yet exposed at the `hs-config` layer. |
| `gc_thresholds` | Unsupported | — | R-PY. Rust has no cyclic tracing GC to tune. |
| `gc_min_interval` | Unsupported | — | R-PY. |
| `filter_timeline_limit` | Unsupported | — | R-PHASE1 (hs-user, sync filters). |
| `block_non_admin_invites` | Unsupported | — | R-PHASE1 (hs-room policy). |
| `enable_search` | Unsupported | — | R-PHASE1 (hs-search). |
| `ip_range_blacklist` | Mapped (diff) | `federation.ip_range_blocklist`, `media.url_preview_ip_range_blocklist` | Synapse applies one list to federation, URL previews, push and identity-server lookups at once; the native schema splits it per subsystem so each can be tuned independently. Federation and media both default to the same private-range list Synapse ships. |
| `ip_range_whitelist` | Mapped (diff) | `federation.ip_range_allowlist` | Same split as above; only the federation half has a native allowlist override today (media preview fetch does not yet have one — see `url_preview_ip_range_whitelist` below). |
| `listeners` | Mapped (diff) | `listeners.listeners[]` | Same shape (bind addresses, port, TLS, resource names, `x_forwarded`) minus the `replication` resource (R-WORKER: no workers) and Synapse's per-resource `additional_resources`. |
| `manhole` | Unsupported | — | R-PROC. No Python-REPL-style debug port. |
| `manhole_settings` | Unsupported | — | R-PROC. |
| `http_proxy` | Unsupported | — | R-PHASE1 (hs-http). Outbound HTTP proxying is planned to honor the standard `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment variables rather than dedicated config keys; not yet implemented either way. |
| `https_proxy` | Unsupported | — | R-PHASE1 (hs-http), paired with `http_proxy`. |
| `no_proxy_hosts` | Unsupported | — | R-PHASE1 (hs-http), paired with `http_proxy`. |
| `matrix_authentication_service` | Mapped (diff) | `auth.mas_delegation` | Field names differ (`secret`/`secret_path` → `shared_secret`/`shared_secret_file`); `enabled` is implicit in the block being present rather than a separate boolean; `force_http2` (H2C to MAS) has no native equivalent. |
| `dummy_events_threshold` | Unsupported | — | R-PHASE1 (hs-room, forward-extremity maintenance). |
| `delete_stale_devices_after` | Unsupported | — | R-PHASE1 (hs-e2e, device hygiene). |
| `email` | Unsupported | — | R-PHASE1 (hs-auth/hs-push). Outbound email delivery (SMTP settings, templates, notification emails) is not implemented in Phase 0; password reset and email notifications are deferred to Phase 1. |
| `max_event_delay_duration` | Unsupported | — | R-PHASE1 (hs-room). MSC4140 delayed events is on the required MSC list (`PLAN.md` 10.4) but its config surface is not yet in `hs-config`. |
| `user_types` | Unsupported | — | R-PHASE1 (hs-auth, custom user-type registry). |

## Homeserver blocking

| Option | Status | Native | Notes |
|---|---|---|---|
| `admin_contact` | Mapped | `server.admin_contact` | |
| `hs_disabled` | Unsupported | — | R-PHASE1 (hs-admin). A global kill switch is a plausible admin-API action, not a static config key, in this design; not yet implemented either way. |
| `hs_disabled_message` | Unsupported | — | R-PHASE1 (hs-admin), paired with `hs_disabled`. |
| `limit_usage_by_mau` | Unsupported | — | R-MAU. |
| `max_mau_value` | Unsupported | — | R-MAU. |
| `mau_trial_days` | Unsupported | — | R-MAU. |
| `mau_appservice_trial_days` | Unsupported | — | R-MAU. |
| `mau_limit_alerting` | Unsupported | — | R-MAU. |
| `mau_stats_only` | Unsupported | — | R-MAU. |
| `mau_limit_reserved_threepids` | Unsupported | — | R-MAU. |
| `server_context` | Unsupported | — | R-MAU. Only used in the MAU-limit-exceeded error message. |
| `limit_remote_rooms` | Unsupported | — | R-PHASE1 (hs-room, join-size policy). |
| `require_membership_for_aliases` | Unsupported | — | R-PHASE1 (hs-room directory policy). |
| `allow_per_room_profiles` | Unsupported | — | R-PHASE1 (hs-room profile policy). |
| `max_avatar_size` | Unsupported | — | R-PHASE1 (hs-media/profile). Not distinguished from `media.max_upload_size` yet. |
| `allowed_avatar_mimetypes` | Unsupported | — | R-PHASE1 (hs-media/profile). |
| `redaction_retention_period` | Unsupported | — | R-PHASE1 (hs-room retention/purge). |
| `redaction_allowed_period` | Unsupported | — | R-PHASE1 (hs-room retention/purge). |
| `forgotten_room_retention_period` | Unsupported | — | R-PHASE1 (hs-room retention/purge). |
| `user_ips_max_age` | Unsupported | — | R-PHASE1 (hs-auth, login-IP retention). |
| `request_token_inhibit_3pid_errors` | Unsupported | — | R-PHASE1 (hs-auth, 3PID enumeration hardening). |
| `next_link_domain_whitelist` | Unsupported | — | R-PHASE1 (hs-auth, password-reset redirect allowlist). |
| `templates` | Unsupported | — | R-PHASE1 (hs-compat/hs-auth). Custom Jinja template overrides for email/SSO pages; the native templating mechanism is not yet designed. |
| `retention` | Unsupported | — | R-PHASE1 (hs-room retention/purge scheduler). |

## TLS

| Option | Status | Native | Notes |
|---|---|---|---|
| `tls_certificate_path` | Mapped (diff) | `listeners.listeners[].tls.certificate_path` | Synapse has one global cert/key pair shared by every `tls: true` listener; the native schema configures TLS per listener. A single-cert Synapse deployment translates to the same path repeated on every listener that had `tls: true`. |
| `tls_private_key_path` | Mapped (diff) | `listeners.listeners[].tls.private_key_path` | Same as above. |
| `federation_verify_certificates` | Mapped | `federation.verify_certificates` | |
| `federation_client_minimum_tls_version` | Unsupported | — | R-PHASE1 (hs-federation). `rustls`'s default TLS 1.2+ floor is used; not yet configurable. |
| `federation_certificate_verification_whitelist` | Unsupported | — | R-SECURITY. A per-domain certificate-verification bypass list is not offered; use `federation.verify_certificates = false` globally (test/private-federation use) or a reverse proxy. |
| `federation_custom_ca_list` | Mapped (diff) | `federation.custom_ca_certificates` | Checked against `crates/hs-federation/src/client.rs` (loads these PEM files into the outbound federation TLS trust store) and `crates/hs-config/src/federation.rs`. Same shape (a flat list of PEM file paths) but different trust semantics: Synapse's `trustRootFromCertificates` *replaces* the platform trust store with exactly this list, while the native field adds these CAs *on top of* the bundled public roots. An operator relying on Synapse's replace-only semantics should review this before cutover. |

## Federation

| Option | Status | Native | Notes |
|---|---|---|---|
| `federation_domain_whitelist` | Mapped | `federation.domain_allowlist` | |
| `federation_whitelist_endpoint_enabled` | Unsupported | — | R-PHASE1 (hs-compat). The `GET /_synapse/client/v1/config/federation_whitelist` diagnostic endpoint is not yet implemented. |
| `federation_metrics_domains` | Unsupported | — | R-PHASE1 (hs-telemetry). Per-domain federation metric breakdown is not yet in the telemetry schema. |
| `allow_profile_lookup_over_federation` | Unsupported | — | R-PHASE1 (hs-federation, profile-query policy). |
| `allow_device_name_lookup_over_federation` | Mapped | `federation.allow_device_name_lookup_over_federation` | |
| `federation` | Mapped (diff) | `federation.client_timeout`, `federation.max_retry_backoff` | Synapse tunes the short-retry (interactive) and long-retry (background) algorithms independently (`max_short_retry_delay`, `max_long_retry_delay`); the native schema exposes one backoff ceiling for both. |

## Caching

| Option | Status | Native | Notes |
|---|---|---|---|
| `event_cache_size` | Unsupported | — | R-PHASE1 (hs-room). Cache sizing is expected to be automatic (working-set based) rather than a fixed entry count; not yet implemented either way. |
| `caches` | Unsupported | — | R-PHASE1. Global/per-cache factor tuning is not yet exposed; lands with the perf-tuning section of `hs-config` once cache implementations exist across tracks. |

## Database

| Option | Status | Native | Notes |
|---|---|---|---|
| `database` | Mapped (diff) | `storage` (`Postgres` variant) | Synapse's `name: sqlite3` has no equivalent (the embedded backend is Fjall, not SQLite); `txn_limit` and `allow_unsafe_locale` are not modeled; `args.cp_max` maps to `storage.postgres.pool_size`. |
| `databases` | Unsupported | — | Database sharding across multiple Postgres hosts by table is superseded by the store's own shard mechanism (`storage.slatedb.shard_count`, `PLAN.md` section 6.5); the Postgres backend uses one database. |

## Logging

| Option | Status | Native | Notes |
|---|---|---|---|
| `log_config` | Mapped (diff) | `telemetry.logging` | Synapse points at an external Python `dictConfig` YAML file with arbitrary handlers and filters; the native schema has a fixed structured-logging output (level plus a JSON/text toggle, `telemetry.logging.level`/`telemetry.logging.json`) rather than arbitrary handler composition. |

## Ratelimiting

| Option | Status | Native | Notes |
|---|---|---|---|
| `rc_message` | Mapped | `rate_limits.message` | |
| `rc_registration` | Mapped | `rate_limits.registration` | |
| `rc_registration_token_validity` | Unsupported | — | R-PHASE1 (hs-auth). Registration tokens are not yet modeled. |
| `rc_login` | Mapped (diff) | `rate_limits.login` | Synapse has three independent buckets (`address`, `account`, `failed_attempts`); the native schema has one. A per-mechanism split is a plausible Phase 1 addition if abuse patterns require it. |
| `rc_admin_redaction` | Mapped | `rate_limits.admin_redaction` | |
| `rc_joins` | Mapped | `rate_limits.joins_local`, `rate_limits.joins_remote` | Synapse's `local`/`remote` split maps 1:1. |
| `rc_joins_per_room` | Unsupported | — | R-PHASE1 (hs-room). Only a per-user join rate is modeled, not a per-room ceiling. |
| `rc_3pid_validation` | Mapped | `rate_limits.third_party_id_validation` | |
| `rc_invites` | Unsupported | — | R-PHASE1 (hs-room). Invite-specific rate limiting is not modeled separately from `rate_limits.message`. |
| `rc_third_party_invite` | Unsupported | — | R-PHASE1 (hs-room). |
| `rc_media_create` | Unsupported | — | R-PHASE1 (hs-media). Upload-specific rate limiting is not modeled separately. |
| `rc_federation` | Mapped (diff) | `rate_limits.federation` | Synapse's five-field sliding-window limiter (`window_size`, `sleep_limit`, `sleep_delay`, `reject_limit`, `concurrent`) is represented as one token bucket. |
| `rc_presence` | Unsupported | — | R-PHASE1 (hs-user). |
| `rc_delayed_event_mgmt` | Unsupported | — | R-PHASE1 (hs-room, MSC4140). |
| `rc_reports` | Unsupported | — | R-PHASE1 (hs-room, content reports). |
| `rc_room_creation` | Unsupported | — | R-PHASE1 (hs-room). |
| `rc_user_directory` | Unsupported | — | R-PHASE1 (hs-search). |
| `federation_rr_transactions_per_room_per_second` | Unsupported | — | R-PHASE1 (hs-federation). Per-room read-receipt relay throttling; `rate_limits.federation` is the closest, coarser analog. |

## Media Store

| Option | Status | Native | Notes |
|---|---|---|---|
| `enable_authenticated_media` | Mapped (diff) | `media.allow_legacy_unauthenticated_media` | Inverted: authenticated media is unconditional here (`PLAN.md` D6); the native flag controls whether the *legacy* unauthenticated endpoints are also served, the mirror image of Synapse's opt-in-to-the-new-thing flag. |
| `enable_media_repo` | Mapped (diff) | `listeners.listeners[].resources` | Disabling the whole media repo is done by omitting the `media` resource from every listener rather than a dedicated flag. |
| `enable_local_media_storage` | Unsupported | — | No equivalent: `media.storage` backend selection is exclusive (local, S3, GCS or Azure), not layered with a separate on/off switch for the local tier. |
| `media_store_path` | Mapped | `media.storage` (`Local` variant `path`) | |
| `max_pending_media_uploads` | Unsupported | — | R-PHASE1 (hs-media, MSC2246 async uploads). |
| `unused_expiration_time` | Unsupported | — | R-PHASE1 (hs-media, MSC2246 async uploads). |
| `media_storage_providers` | Mapped (diff) | `media.storage` | Synapse's pluggable provider-module list (including the S3 storage provider) collapses to one native backend selection; `object_store` already covers S3/GCS/Azure/local (`PLAN.md` D6). Multiple simultaneous providers or write-through caching between them are not supported. |
| `max_upload_size` | Mapped | `media.max_upload_size` | |
| `media_upload_limits` | Unsupported | — | R-PHASE1 (hs-media). Per-mimetype upload size limits are not modeled; only the global `media.max_upload_size`. |
| `max_image_pixels` | Unsupported | — | R-PHASE1 (hs-media, decompression-bomb protection). |
| `remote_media_download_burst_count` | Unsupported | — | R-PHASE1 (hs-media). |
| `remote_media_download_per_second` | Unsupported | — | R-PHASE1 (hs-media). |
| `prevent_media_downloads_from` | Unsupported | — | R-PHASE1 (hs-media, per-server remote-media block list). |
| `dynamic_thumbnails` | Unsupported | — | R-PHASE1 (hs-media). Only the fixed `media.thumbnail_sizes` list is served; arbitrary on-the-fly sizes are not. |
| `thumbnail_sizes` | Mapped | `media.thumbnail_sizes` | |
| `media_retention` | Mapped (diff) | `media.remote_media_retention` | Synapse also has `local_media_lifetime`; the native schema only models remote-media retention so far. |
| `url_preview_enabled` | Mapped | `media.url_preview_enabled` | |
| `url_preview_ip_range_blacklist` | Mapped | `media.url_preview_ip_range_blocklist` | |
| `url_preview_ip_range_whitelist` | Unsupported | — | R-PHASE1 (hs-media). No allowlist override for preview fetches yet (unlike `federation.ip_range_allowlist`). |
| `url_preview_url_blacklist` | Unsupported | — | R-PHASE1 (hs-media). URL/domain-pattern blocklist, distinct from the IP-range blocklist. |
| `max_spider_size` | Mapped | `media.url_preview_max_fetch_size` | Checked against `crates/hs-media/src/preview.rs`'s `FetchLimits::max_body_bytes` (still a hardcoded 10 MiB constant as of this check, not yet reading this field — see `docs/status/13-config-compat-and-migration.md` for the one-line consumer change 09 still needs) and Synapse's own default (`"10M"`, `refs/synapse/synapse/config/repository.py`), which the native default matches. |
| `url_preview_accept_language` | Unsupported | — | R-PHASE1 (hs-media). |
| `oembed` | Unsupported | — | R-PHASE1 (hs-media, oEmbed provider list). |

## Captcha

| Option | Status | Native | Notes |
|---|---|---|---|
| `recaptcha_public_key` | Unsupported | — | R-PHASE1 (hs-auth). CAPTCHA-gated registration is not in the Phase 0 schema; tracked alongside registration tokens as a registration-hardening addition. |
| `recaptcha_public_key_path` | Unsupported | — | R-PHASE1 (hs-auth), paired with above. |
| `recaptcha_private_key` | Unsupported | — | R-PHASE1 (hs-auth). |
| `recaptcha_private_key_path` | Unsupported | — | R-PHASE1 (hs-auth). |
| `enable_registration_captcha` | Unsupported | — | R-PHASE1 (hs-auth). |
| `recaptcha_siteverify_api` | Unsupported | — | R-PHASE1 (hs-auth). |

## TURN

| Option | Status | Native | Notes |
|---|---|---|---|
| `turn_uris` | Unsupported | — | R-PHASE1. TURN/MatrixRTC credential issuance (MSC4143 is on the required MSC list, `PLAN.md` 10.4) does not yet have an `hs-config` surface. |
| `turn_shared_secret` | Unsupported | — | R-PHASE1, paired with `turn_uris`. |
| `turn_shared_secret_path` | Unsupported | — | R-PHASE1, paired with `turn_uris`. |
| `turn_username` | Unsupported | — | R-PHASE1, paired with `turn_uris`. |
| `turn_password` | Unsupported | — | R-PHASE1, paired with `turn_uris`. |
| `turn_user_lifetime` | Unsupported | — | R-PHASE1, paired with `turn_uris`. |
| `turn_allow_guests` | Unsupported | — | R-PHASE1, paired with `turn_uris`. |
| `matrix_rtc` | Unsupported | — | R-PHASE1. MatrixRTC transport/SFU configuration for MSC4143; not yet in `hs-config`. |

## Registration

| Option | Status | Native | Notes |
|---|---|---|---|
| `enable_registration` | Mapped | `auth.enable_registration` | |
| `enable_registration_without_verification` | Unsupported | — | R-PHASE1 (hs-auth). |
| `registrations_require_3pid` | Unsupported | — | R-PHASE1 (hs-auth). |
| `disable_msisdn_registration` | Unsupported | — | R-PHASE1 (hs-auth). |
| `allowed_local_3pids` | Unsupported | — | R-PHASE1 (hs-auth). |
| `enable_3pid_lookup` | Unsupported | — | R-PHASE1 (hs-auth). |
| `registration_requires_token` | Unsupported | — | R-PHASE1 (hs-auth, registration tokens). |
| `registration_shared_secret` | Mapped | `auth.registration_shared_secret` | Also see `docs/compat/synapse-admin-routes.md` and the shared-secret registration protocol in `hs-compat`. |
| `registration_shared_secret_path` | Mapped | `auth.registration_shared_secret_file` | |
| `bcrypt_rounds` | Unsupported | — | R-PHASE1 (hs-auth). Password hashing cost factor is not yet tunable; a fixed, at-least-as-strong default is used. |
| `allow_guest_access` | Unsupported | — | R-PHASE1 (hs-auth). |
| `default_identity_server` | Unsupported | — | R-PHASE1 (hs-auth/hs-identity). |
| `account_threepid_delegates` | Unsupported | — | R-PHASE1 (hs-auth). |
| `enable_set_displayname` | Unsupported | — | R-PHASE1 (hs-user/profile). |
| `enable_set_avatar_url` | Unsupported | — | R-PHASE1 (hs-user/profile). |
| `enable_3pid_changes` | Unsupported | — | R-PHASE1 (hs-auth). |
| `auto_join_rooms` | Unsupported | — | R-PHASE1 (hs-room/hs-auth). |
| `autocreate_auto_join_rooms` | Unsupported | — | R-PHASE1, paired with `auto_join_rooms`. |
| `autocreate_auto_join_rooms_federated` | Unsupported | — | R-PHASE1, paired with `auto_join_rooms`. |
| `autocreate_auto_join_room_preset` | Unsupported | — | R-PHASE1, paired with `auto_join_rooms`. |
| `auto_join_mxid_localpart` | Unsupported | — | R-PHASE1, paired with `auto_join_rooms`. |
| `auto_join_rooms_for_guests` | Unsupported | — | R-PHASE1, paired with `auto_join_rooms`. |
| `inhibit_user_in_use_error` | Unsupported | — | R-PHASE1 (hs-auth). |
| `allow_underscore_prefixed_registration` | Unsupported | — | R-PHASE1 (hs-auth). |

## User session management

| Option | Status | Native | Notes |
|---|---|---|---|
| `session_lifetime` | Unsupported | — | R-PHASE1 (hs-auth). No equivalent absolute session ceiling independent of token lifetimes. |
| `refreshable_access_token_lifetime` | Mapped (diff) | `auth.access_token_lifetime` | Synapse distinguishes refreshable vs. non-refreshable token lifetimes; the native schema has one `access_token_lifetime` for native OAuth tokens. |
| `refresh_token_lifetime` | Mapped | `auth.refresh_token_lifetime` | |
| `nonrefreshable_access_token_lifetime` | Unsupported | — | R-PHASE1 (hs-auth). Legacy non-refreshing logins reuse `auth.access_token_lifetime`. |
| `ui_auth` | Unsupported | — | R-PHASE1 (hs-auth, UIA session timeout). |
| `login_via_existing_session` | Unsupported | — | R-PHASE1 (hs-auth). |

## Metrics

| Option | Status | Native | Notes |
|---|---|---|---|
| `enable_metrics` | Mapped | `telemetry.metrics.enabled` | |
| `sentry` | Mapped (diff) | `telemetry.sentry` | `dsn`/`dsn_path` → `dsn`/`dsn_file`; arbitrary extra Sentry SDK kwargs Synapse passes through are not supported. |
| `metrics_flags` | Unsupported | — | R-PHASE1 (hs-telemetry). Per-metric detail toggles (e.g. `known_servers`) not yet exposed. |
| `report_stats` | Mapped | `server.report_stats` | |
| `report_stats_endpoint` | Unsupported | — | R-PHASE1 (hs-telemetry). Custom stats-reporting URL override. |

## API Configuration

| Option | Status | Native | Notes |
|---|---|---|---|
| `room_prejoin_state` | Unsupported | — | R-PHASE1 (hs-room). |
| `track_puppeted_user_ips` | Unsupported | — | R-PHASE1 (hs-appservice/hs-auth). |
| `app_service_config_files` | Mapped | `appservices.registration_files` | |
| `track_appservice_user_ips` | Unsupported | — | R-PHASE1 (hs-appservice). |
| `use_appservice_legacy_authorization` | Unsupported | — | R-SECURITY. Only the `Authorization: Bearer` header form of appservice auth is supported; the insecure legacy `access_token` query-parameter form is not offered, matching Synapse's own recommendation against it. |
| `macaroon_secret_key` | Mapped (diff) | `auth.session_secret` | Synapse's macaroon key signs guest tokens, SSO short-term login tokens and email-unsubscribe tokens using the macaroon caveat scheme specifically; the native session secret signs native OAuth-issued tokens with a different (non-macaroon) scheme. Same operational role (rotate and every session in flight is invalidated), different format. |
| `macaroon_secret_key_path` | Mapped | `auth.session_secret_file` | |
| `form_secret` | Unsupported | — | R-PHASE1 (hs-auth). CSRF-form-signing secret for the SSO fallback login page; the native SSO page implementation does not exist yet. |
| `form_secret_path` | Unsupported | — | R-PHASE1, paired with `form_secret`. |

## Signing Keys

| Option | Status | Native | Notes |
|---|---|---|---|
| `signing_key_path` | Mapped (diff) | `server.signing_key_path` | Synapse points at one file containing one active signing key; the native field is a directory, since key rotation needs more than one active key at once (see the doc comment on `ServerConfig::signing_key_path`). |
| `old_signing_keys` | Unsupported | — | R-PHASE1 (hs-federation). Publishing retired keys so others can still verify old signatures is not yet modeled; tracked with key-rotation tooling. |
| `key_refresh_interval` | Unsupported | — | R-PHASE1 (hs-federation, remote key cache TTL). |
| `trusted_key_servers` | Unsupported | — | R-PHASE1 (hs-federation, notary/perspective key server list). |
| `suppress_key_server_warning` | Unsupported | — | R-PHASE1, paired with `trusted_key_servers`; not applicable until that lands. |
| `key_server_signing_keys_path` | Unsupported | — | R-PHASE1 (hs-federation, acting as a notary server for others). |

## Single sign-on integration

| Option | Status | Native | Notes |
|---|---|---|---|
| `saml2_config` | Unsupported | — | R-PHASE1 (hs-auth). SAML 2.0 SP support is not implemented in the Phase 0 auth schema, which covers OIDC upstream IdPs only (`PLAN.md` D5); tracked for Phase 1. |
| `oidc_providers` | Mapped (diff) | `auth.oidc_providers` | Fewer sub-options than Synapse's (no per-provider claim-mapping template, no `user_mapping_provider` Python module hook — see `modules`/R-MODULE); `idp_id`, `issuer`, `client_id`, `client_secret`/`client_secret_path` and `scopes` map directly. |
| `cas_config` | Unsupported | — | R-SSO-LEGACY. |
| `sso` | Unsupported | — | R-PHASE1 (hs-auth). SSO landing-page customization (`client_whitelist`, template overrides, `update_profile_information`) is not yet in the schema; tracked with `templates`. |
| `jwt_config` | Unsupported | — | R-PHASE1 (hs-auth). `m.login.jwt` is not implemented in the Phase 0 auth schema. |
| `password_config` | Mapped (diff) | `auth.password` | Synapse's `localdb_enabled` (disable the local password DB while keeping password login via a custom Python provider) has no equivalent — see `modules`/R-MODULE. `enabled`, `pepper`/`pepper_path` and `policy` map directly. |

## Push

| Option | Status | Native | Notes |
|---|---|---|---|
| `push` | Unsupported | — | R-PHASE1 (hs-push). Delivery tuning (`include_content`, `group_unread_count_by_room`, jitter) not yet in `hs-config`. |
| `push_rules` | Unsupported | — | R-PHASE1 (hs-push). Server-set default push-rule overrides. |

## Rooms

| Option | Status | Native | Notes |
|---|---|---|---|
| `encryption_enabled_by_default_for_room_type` | Unsupported | — | R-PHASE1 (hs-room). |
| `user_directory` | Unsupported | — | R-PHASE1 (hs-search). |
| `user_consent` | Unsupported | — | R-PHASE1 (hs-auth). Template-driven consent-tracking flow; low priority. |
| `stats` | Unsupported | — | R-PHASE1 (hs-room, room/user stats collection toggle). |
| `server_notices` | Unsupported | — | R-PHASE1 (hs-room, server-notices bot). |
| `enable_room_list_search` | Unsupported | — | R-PHASE1 (hs-room directory). |
| `alias_creation_rules` | Unsupported | — | R-PHASE1 (hs-room directory policy). |
| `room_list_publication_rules` | Unsupported | — | R-PHASE1 (hs-room directory policy). |
| `default_power_level_content_override` | Unsupported | — | R-PHASE1 (hs-room). |
| `forget_rooms_on_leave` | Unsupported | — | R-PHASE1 (hs-room). |
| `exclude_rooms_from_sync` | Unsupported | — | R-PHASE1 (hs-user/hs-room). |
| `exclude_rooms_from_presence` | Unsupported | — | R-PHASE1 (hs-user). |

## Opentracing

| Option | Status | Native | Notes |
|---|---|---|---|
| `opentracing` | Mapped (diff) | `telemetry.tracing` | Synapse's block has per-homeserver allow lists and user-keyed sampling policy tied to its Jaeger-specific integration; the native schema has one OTLP endpoint plus a global `sample_ratio`, exported through standard OpenTelemetry (`PLAN.md` 5.5, `hs-telemetry`) so any OTLP-compatible collector works, not just Jaeger. |

## Coordinating workers

| Option | Status | Native | Notes |
|---|---|---|---|
| `worker_replication_secret` | Unsupported | — | R-WORKER. |
| `worker_replication_secret_path` | Unsupported | — | R-WORKER. |
| `start_pushers` | Unsupported | — | R-WORKER. |
| `pusher_instances` | Unsupported | — | R-WORKER. |
| `send_federation` | Unsupported | — | R-WORKER. |
| `federation_sender_instances` | Unsupported | — | R-WORKER. |
| `instance_map` | Unsupported | — | R-WORKER. |
| `stream_writers` | Unsupported | — | R-WORKER. |
| `outbound_federation_restricted_to` | Unsupported | — | R-WORKER. |
| `run_background_tasks_on` | Unsupported | — | R-WORKER. |
| `update_user_directory_from_worker` | Unsupported | — | R-WORKER. |
| `notify_appservices_from_worker` | Unsupported | — | R-WORKER. |
| `media_instance_running_background_jobs` | Unsupported | — | R-WORKER. |
| `redis` | Unsupported | — | R-WORKER. The internal mesh (`cluster.mesh`) replaces Redis pub/sub as the replica-to-replica transport, but is not a like-for-like config mapping (different protocol, different purpose — ownership/forwarding, not a generic bus). |

## Individual worker configuration

| Option | Status | Native | Notes |
|---|---|---|---|
| `worker_app` | Unsupported | — | R-WORKER. |
| `worker_name` | Unsupported | — | R-WORKER. |
| `worker_listeners` | Unsupported | — | R-WORKER. |
| `worker_manhole` | Unsupported | — | R-WORKER, R-PROC. |
| `worker_daemonize` | Unsupported | — | R-WORKER, R-PROC. |
| `worker_pid_file` | Unsupported | — | R-WORKER, R-PROC. |
| `worker_log_config` | Unsupported | — | R-WORKER. |

## Background Updates

| Option | Status | Native | Notes |
|---|---|---|---|
| `background_updates` | Unsupported | — | Schema and data migrations run as bounded, transactional `hs-tables` migrations (`PLAN.md` section 5.5), not Python-style throttled background updates that run for hours after an upgrade; there is no equivalent throttle knob to translate. |

## Auto Accept Invites

| Option | Status | Native | Notes |
|---|---|---|---|
| `auto_accept_invites` | Unsupported | — | R-PHASE1 (hs-room/hs-appservice). Useful for bridges; a plausible near-term addition, not yet in `hs-config`. |

---

## `experimental_features` flags

Synapse gates unreleased or optional protocol features behind `experimental_features.<flag>`. This server has no equivalent gate (`PLAN.md` D10): a feature, once implemented, is either shipped unconditionally (R-NOFLAG) or not implemented at all (R-NOTPLANNED, tracked on demand). The one exception PLAN.md itself calls out is MSC4242 (state DAGs), which section 10.4 explicitly says ships "experimental" — that one is expected to gain a real opt-in flag and is marked accordingly.

| Flag | Status | Native | Notes |
|---|---|---|---|
| `msc1763_enabled` | Unsupported | — | R-NOFLAG. Room retention policies; see `retention` above (R-PHASE1). |
| `msc1767_enabled` | Unsupported | — | R-NOTPLANNED. Extensible events. |
| `msc2409_to_device_messages_enabled` | Unsupported | — | R-NOFLAG. Appservice family, required (`PLAN.md` 10.4). |
| `msc2654_enabled` | Unsupported | — | R-NOFLAG. Unread notification counts are unconditional once `/sync` ships. |
| `msc2815_enabled` | Unsupported | — | R-NOFLAG. View-redacted-content, required (`PLAN.md` 10.4). |
| `msc3026_enabled` | Unsupported | — | R-NOTPLANNED. Busy presence state. |
| `msc3202_transaction_extensions` | Unsupported | — | R-NOFLAG. Appservice family, required. |
| `msc3381_polls_enabled` | Unsupported | — | R-NOTPLANNED. Polls are primarily a client/event-type feature; no server gate planned. |
| `msc3391_enabled` | Unsupported | — | R-NOFLAG. Account-data deletion, required. |
| `msc3575_enabled` | Unsupported | — | R-NOTPLANNED. Legacy (non-simplified) sliding sync; superseded in the spec by MSC4186, which is what `PLAN.md` 10.4 commits to. |
| `msc3664_enabled` | Unsupported | — | R-NOTPLANNED. Push rule for `m.in_reply_to`. |
| `msc3720_enabled` | Unsupported | — | R-NOTPLANNED. Account status lookup. |
| `msc3773_enabled` | Unsupported | — | R-NOFLAG. Thread-unread notifications are a stable spec feature, served unconditionally once sync/threads ship. |
| `msc3814_enabled` | Unsupported | — | R-NOFLAG. Dehydrated devices, required. |
| `msc3848_enabled` | Unsupported | — | R-NOFLAG. Additional standard error codes are used unconditionally. |
| `msc3861` | Mapped (diff) | `auth.mas_delegation`, native OAuth issuer | Superseded by the native OAuth 2.0 authorization server (`PLAN.md` D5) and `auth.mas_delegation` for delegation mode; msc3861's own sub-keys (introspection endpoint, client ID, etc.) are subsumed by those two mechanisms rather than translated field-for-field. |
| `msc3866` | Unsupported | — | R-PHASE1 (hs-auth). Registration-token gating is not yet in the Phase 0 schema (see `registration_requires_token`). |
| `msc3874_enabled` | Unsupported | — | R-NOTPLANNED. Filtering `/messages` by relation type. |
| `msc3881_enabled` | Unsupported | — | R-NOFLAG. Pusher enable/disable, required. |
| `msc3890_enabled` | Unsupported | — | R-NOTPLANNED. Remote push toggle on logout. |
| `msc3912_enabled` | Unsupported | — | R-NOTPLANNED. Relation-based redactions. |
| `msc3983_appservice_otk_claims` | Unsupported | — | R-NOFLAG. Appservice family, required. |
| `msc3984_appservice_key_query` | Unsupported | — | R-NOFLAG. Appservice family, required. |
| `msc4028_push_encrypted_events` | Unsupported | — | R-NOFLAG. Push for encrypted events, required. |
| `msc4069_profile_inhibit_propagation` | Unsupported | — | R-NOTPLANNED. |
| `msc4076_enabled` | Unsupported | — | R-NOTPLANNED. `invite_room_state` on knocks. |
| `msc4108_delegation_endpoint` | Unsupported | — | R-NOFLAG. QR login rendezvous, required. |
| `msc4108_enabled` | Unsupported | — | R-NOFLAG. QR login rendezvous, required. |
| `msc4133_enabled` | Unsupported | — | R-NOFLAG. Extended profiles, required. |
| `msc4143_enabled` | Unsupported | — | R-NOFLAG. MatrixRTC, required. |
| `msc4155_enabled` | Unsupported | — | R-NOFLAG. Invite filtering, required. |
| `msc4169_enabled` | Unsupported | — | R-NOTPLANNED. |
| `msc4210_enabled` | Unsupported | — | R-NOTPLANNED. Removal of legacy `m.room.aliases` from state. |
| `msc4222_enabled` | Unsupported | — | R-NOFLAG. `state_after`, required. |
| `msc4235_enabled` | Unsupported | — | R-NOTPLANNED. `via` field on membership events. |
| `msc4242_enabled` | Unsupported | — | R-PHASE1 (hs-state), uniquely expected to gain a real opt-in flag: `PLAN.md` 10.4 names MSC4242 state DAGs as the one feature that ships "experimental" here too, unlike every other MSC in this table. Not yet in `hs-config`; when added it will not use `experimental_features`-style nesting, just its own key (e.g. under a future `federation` or `rooms` section). |
| `msc4263_limit_key_queries_to_users_who_share_rooms` | Unsupported | — | R-NOTPLANNED. |
| `msc4267_enabled` | Unsupported | — | R-NOTPLANNED. |
| `msc4277_enabled` | Unsupported | — | R-NOTPLANNED. |
| `msc4293_enabled` | Unsupported | — | R-NOTPLANNED. Redact-on-kick/ban. |
| `msc4306_enabled` | Unsupported | — | R-NOFLAG. Thread subscriptions, required. |
| `msc4354_enabled` | Unsupported | — | R-NOFLAG. Sticky events, required. |
| `msc4370_enabled` | Unsupported | — | R-NOTPLANNED. |
| `msc4388_enabled` | Unsupported | — | R-NOTPLANNED. |
| `msc4388_mode` | Unsupported | — | R-NOTPLANNED, paired with `msc4388_enabled`. |
| `msc4446_enabled` | Unsupported | — | R-NOTPLANNED. |
| `msc4450_enabled` | Unsupported | — | R-NOTPLANNED. |
| `msc4452_enabled` | Unsupported | — | R-NOTPLANNED. |
| `msc4491_enabled` | Unsupported | — | R-NOTPLANNED. |
| `msc4502_enabled` | Unsupported | — | R-NOTPLANNED. |
| `msc4512_enabled` | Unsupported | — | R-NOTPLANNED. |
