# 0034. A setting nothing reads gets a reader or leaves the schema, and a removed setting is dropped with a warning

Date: 2026-10-08. Status: accepted. Tracks: 16 (web), 15 (admin API), 13 (configuration and
Synapse compatibility), 07 (auth), 09 (media).

## Context

Decision 0016's amendment found eight settings read by nothing: `server.admin_contact`,
`server.report_stats`, `media.remote_media_retention`, `rate_limits.third_party_id_validation`,
`auth.enable_legacy_login`, `auth.password.enabled`, `auth.session_secret(_file)` and
`appservices.enabled`. The Configuration page showed each as editable while its description
said a change had no effect. The owner's rule (every admin feature has its UI, explained
inline) does not stretch to a switch that does nothing.

## Decision

1. **Each such setting gets a reader or leaves the schema.**
   - Read now: `server.admin_contact` is `GET /.well-known/matrix/support` (spec v1.10:
     an email address or Matrix ID as the `m.role.admin` contact, an http(s) address as
     `support_page`; `hs_cli::well_known`), hot. `auth.password.enabled` is read by
     `GET /login` (not offered) and `POST /login` (`403 M_FORBIDDEN`), hot; re-authenticating
     with a password for a sensitive change still works, as Synapse's `only_for_reauth`.
     `media.remote_media_retention` is read by an hourly sweeper (`hs_media::retention`) that
     deletes cached remote copies unused for that long, keeping protected and quarantined ones,
     as `media.purge_remote_cache` selects them. `rate_limits.third_party_id_validation` already
     had a reader (`hs_auth::threepid`) by this date.
   - Removed: `server.report_stats` (nothing will send statistics), `auth.enable_legacy_login`
     (the classic sign-in is always served; `auth.mas_delegation` is the way to hand sign-in to
     another issuer), `auth.session_secret` and `_file` (sessions, tokens and sign-in state live
     in the store; nothing is signed with a shared secret), `appservices.enabled` (bridges are
     paused one at a time on their pages).
2. **A removed setting does not stop a configuration that carries it from loading.**
   `hs_config::retired::RETIRED_SETTINGS` lists them; `Config::from_value`, `Layers::merged`
   and the first-run seeding drop them and log a `warn` (target `hs_config::retired`) once per
   setting per process, naming what to do instead. A new write of one through the admin API
   (`config.update`, `config.validate`) is a `400` naming the setting and why it went.
3. **The Synapse translator** maps `report_stats`, `macaroon_secret_key` and
   `macaroon_secret_key_path` to nothing, as `Mapped (diff)` rows whose note starts "Not
   needed", so they do not block a translation; `password_config.enabled: only_for_reauth`
   becomes `auth.password.enabled: false`.

## Consequences

- `appservices` is now a reloadable section (its administered settings are all hot).
- A setting later found to have no reader follows the same rule: give it one in the change
  that finds it, or add it to `RETIRED_SETTINGS` and remove the field.
- `crates/hs-federation/scripts/two-server-federation.sh` still writes
  `auth.enable_legacy_login: true`; it loads, with the warning.
