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
