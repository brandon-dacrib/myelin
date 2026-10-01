# 0016. A configuration change applies at once where something re-reads it, and the server-wide send limit is enforced

Date: 2026-09-28. Status: accepted. Tracks: 15 (admin API), 13 (configuration), 04 (rooms), 12
(platform), 14 (test harnesses).

## Context

`hs_config::reload` listed `rate_limits`, `federation`, `telemetry`, `appservices` and
`migration` as reloadable, and the management interface told operators that saving those
sections "applies it to the running server straight away". Nothing in `hs serve` re-read any of
them: the federation client, the logging layer and the appservice scheduler are built once at
startup, and `rate_limits` was not read at all, because no route enforced the server-wide limit
(decision 0014 left that open, since it changes every client's pace). `config.reload` said so
honestly by never listing a section as reloaded, which made it honest and useless.

## Decision

1. **The reload boundary states what the server does.** `hs_config::reload::HOT_SETTINGS` lists
   the settings something in the running process re-reads, as JSON Pointers (a section or a
   single setting); `RELOADABLE_SECTIONS` is the sections every setting of which is hot. A
   setting joins the list in the same change that wires something to re-read it.
   `sections_requiring_restart` compares with hot settings taken out, so a section with some hot
   settings needs a restart only when one of its other settings changed.
2. **One choke point applies changes.** `hs_cli::live_config::LiveConfig` holds what each hot
   section's re-readers registered at startup (`on_change`). The store-backed configuration
   source applies every configuration it reads back after a write (`refresh`, which update,
   reload and any later write path such as a revert all go through), so no HTTP handler has to
   remember to. Each section applied, failed or unwired is logged and counted in
   `hs_config_reloads_total{section,outcome}`. A section whose applier fails keeps its old value
   and is tried again on the next write or ten-second follower tick, even at the same revision.
3. **Answers carry what happened.** `config.update`'s answer has `applied` (a
   `ConfigReloadReport`: `reloaded_sections`, `requires_restart`, `errors`); `config.reload`
   answers the same shape; `config.validate`'s `requires_restart` uses the same boundary.
4. **The server-wide send limit is enforced.** `rate_limits.message` (while `rate_limits.enabled`)
   limits every sender without an administrator's override, on sending, state events and
   redactions, in `hs_room::moderation::SendLimiter` -- the same bucket, the same route checks and
   the same `429 M_LIMIT_EXCEEDED` as the override of decision 0014. Appservices registered with
   `rate_limited: false` are exempt. When the limit changes, a sender keeps what is left of their
   bucket, clamped to the new burst, so lowering it bites at once. The other `rate_limits` buckets
   are still not enforced anywhere.
5. **Harnesses that send faster than people do switch it off**, as Synapse's Complement image
   does: `tests/complement/startup.sh` and `hs-loadgen` write `rate_limits: {enabled: false}`.
   `hs-room`'s own tests, and anything that builds a `RoomRegistry` without `hs serve`, have no
   server-wide limit at all.

## Consequences

- A default `hs serve` now refuses an eleventh message within a couple of seconds from one user
  (Synapse's defaults: 0.2 per second, burst 10). Clients handle `429` with `retry_after_ms`;
  an operator who wants otherwise changes `rate_limits` in the interface, which applies at once.
- In cluster mode a change is applied at once by the replica that took the write. Every replica
  also checks the store's revision every ten seconds (`StoreConfigSource::follow_store`) and
  applies what moved, so the others follow within that long; so does a server whose store `hs
  config` wrote to directly.
- Federation allow/block lists and the telemetry log filter apply immediately. Other
  federation and telemetry settings, and appservice settings, need a restart; each joins
  `HOT_SETTINGS` when a running component starts re-reading it.

## Amendment, 2026-10-01: every setting is classified, and most are hot

Branch `agent/config-hot`. Tracks: 13 (configuration), 07 (auth), 04 (rooms), 06 (federation),
09 (media), 11 (appservices), 15 (admin API), 16 (web).

1. **One table classifies every setting.** `hs_config::reload::SETTINGS` gives each setting (a
   JSON Pointer; no entry beneath another) one of three kinds -- `bootstrap`, `hot`, `restart`
   -- and what reads it. `HOT_SETTINGS`, `RELOADABLE_SECTIONS` (now: sections in which every
   *administered* setting is hot) and `applies(pointer)` derive from it; its bootstrap entries
   must equal `bootstrap::BOOTSTRAP_SETTINGS`. A test walks the derived JSON Schema
   (`hs_config::schema::field_pointers`, through `$ref`s, optional structures and every variant)
   and fails on any setting that is not covered exactly once, so a new setting cannot ship
   unclassified.
2. **The classification is published, not copied.** `hs_config::schema::json_schema()` is the
   derived schema with `"x-applies": "bootstrap" | "hot" | "restart"` on each classified
   property; the admin API serves it as `GET /config/schema`'s `schema`, adds `applies` to each
   `ConfigSettingInfo` (and derives `reloadable` from it), `docs/config.md` has an Applies column
   generated from it, and the web fixture is regenerated from it. The interface's mock reads its
   hot and bootstrap settings from that fixture's `x-applies` rather than keeping a list.
3. **Counts at this amendment:** 7 bootstrap, 39 hot, 25 restart (71 entries).
   - Bootstrap: `server.server_name`, `server.signing_key_path`, `listeners`, `storage`,
     `cluster.single_node`, `cluster.mesh`, `appservices.registration_files`.
   - Hot: `server.public_baseurl`, `well_known_server`, `unstable_features`, `admin_contact`,
     `report_stats`; `media.max_upload_size`, `thumbnail_sizes`, the five `url_preview_*`
     settings, `remote_media_retention`; the federation allow/block lists and
     `allow_public_rooms_over_federation`, `allow_device_name_lookup_over_federation`; every
     `rate_limits` setting; `auth.enable_registration`, `registration_shared_secret(_file)`,
     `user_directory_search_all_users`, `access_token_lifetime`, `refresh_token_lifetime`,
     `password.pepper(_file)`, `password.policy`; `appservices.tracking_failure_threshold`;
     `telemetry.logging.level`; `migration.synapse`.
   - Restart: `media.storage`, `media.scanning`, `media.allow_legacy_unauthenticated_media` (the
     legacy routes are mounted or not); `federation.enabled`, `verify_certificates`,
     `custom_ca_certificates`, `trust_os_root_store`, `client_timeout`, `max_retry_backoff`,
     `max_queued_pdus_per_destination`; `auth.enable_legacy_login`, `session_secret(_file)`,
     `password.enabled`, `oidc_providers`, `mas_delegation`; `appservices.enabled`;
     `telemetry.metrics`, `tracing`, `logging.json`, `sentry`; `cluster.room_shards`,
     `user_shards`, `heartbeat_interval`, `lease_ttl`.
   - Read by nothing at all yet (a change has no effect either way): `server.admin_contact`,
     `server.report_stats`, `media.remote_media_retention`,
     `rate_limits.third_party_id_validation` (no requestToken route is served) -- classified hot;
     `auth.enable_legacy_login`, `auth.password.enabled`, `auth.session_secret`,
     `appservices.enabled` -- classified restart.
4. **Readers hold a live handle.** `hs_config::Live<T>` (a shared `RwLock<Arc<T>>`) is what a
   reader of a hot setting holds: `hs_auth::AuthState::config` (with `set_config`, keeping the
   server name), `hs_media::MediaRepository`'s configuration and thumbnail table (`set_config`),
   the `.well-known` documents and `/versions`' unstable features. Flags and counters that are
   one value are atomics (`hs_federation::transport::InboundPolicy`,
   `hs_appservice::Registry::set_failure_threshold`). `hs serve` registers one applier per
   section with `LiveConfig::on_change`, as before.
5. **Every `rate_limits` bucket is enforced** (this supersedes point 4's "the other buckets are
   still not enforced anywhere"), with `hs_http::buckets::TokenBuckets` -- keyed buckets whose
   limit is swapped while the server runs, keeping what a key has left clamped to the new burst:
   `login` (every `POST /login` but an appservice's, per client address), `registration`
   (checked on every `POST /register`, taken when an account is made, per client address;
   appservices exempt), `joins_local` and `joins_remote` (per user, appservices with
   `rate_limited: false` exempt), `admin_redaction` (a server administrator's redactions, in
   place of the message limit, unless an override applies), `federation` (inbound `PUT /send`,
   per verified origin). All per replica, like `message`. Refusals are `429 M_LIMIT_EXCEEDED`
   with `retry_after_ms`, counted in `hs_rate_limited_total{bucket}`.
6. **The client address** for the per-address buckets is `hs_http::buckets::ClientIp`: the first
   `X-Forwarded-For` address when the listener has `x_forwarded: true` or the peer is a loopback
   or private address (a proxy or ingress in front of the server -- otherwise one proxy's address
   would be one bucket for every client); else the peer; and no key at all for a loopback peer
   forwarding nothing (this host's own tooling, health checks, tests) or a request with no peer
   (replayed over the cluster mesh). Listeners now serve with `ConnectInfo<SocketAddr>`.
7. **Per-setting observability.** Each changed hot setting is logged ("configuration setting
   applied to the running server", `setting=<pointer>`) and counted in
   `hs_config_settings_applied_total{setting,outcome}` (`applied`, `failed`, `unwired`), beside
   the per-section `hs_config_reloads_total`.

Consequence: a server behind a proxy that forwards nothing (no `X-Forwarded-For`) from a public
address puts all of its clients in one login and one registration bucket (Synapse's defaults: 3
at once, then one every six seconds). Such a proxy should forward the client's address; a
private-address proxy's is believed without `x_forwarded`.
