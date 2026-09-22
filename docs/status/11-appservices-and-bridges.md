# Status: track 11, appservices and bridges

Last updated: 2026-09-22.

**A real bridge works.** heisenbridge, against the real binary and a local IRC server: it
registers its bot, drives the server with masqueraded requests, receives every event in its
rooms as transactions, relays IRC to Matrix through ghost users and Matrix to IRC. Reproduction
and the exact list of what was and was not verified: `docs/bridges/heisenbridge.md`. Two things
had to be fixed for it to get past its first request -- `/register` had no
`m.login.application_service` branch, and `hs serve` delivered no events to any appservice
(there was a scheduler and nothing that fed it; `src/pump.rs` and `src/delivery.rs` now do) --
and two more were found on the way: a server with a registration file in its configuration
could not start a second time, and after any restart nothing said in a pre-existing room reached
`/sync`, push or a bridge. All four are fixed, each with a test that fails without it.

## Done

`crates/hs-appservice` (library; all `cargo test -p hs-appservice` green, 74 tests; `cargo clippy
-p hs-appservice --all-targets -- -D warnings` clean; `cargo fmt -p hs-appservice` applied):

- **Registration file parser** (`src/registration.rs`, `src/namespace.rs`, `src/regexp.rs`):
  every field `PLAN.md` Appendix B lists — `id`, `url` (nullable, distinct from missing),
  `as_token`, `hs_token`, `sender_localpart`, `rate_limited`, `namespaces.{users,aliases,rooms}`
  with `regex`/`exclusive`, `protocols`, `receive_ephemeral`,
  `de.sorunome.msc2409.push_ephemeral` (tracked as **two independent booleans**, matching
  Synapse's own loader exactly — see "Decisions made"), `org.matrix.msc3202`,
  `io.element.msc4190`. Unrecognized top-level keys (e.g. `com.beeper.*`) are preserved in
  `Registration::extra` and round-tripped through `to_yaml()` export. Namespace regexes use Python
  `re.match` (prefix) semantics via a `regex`-crate-first, `fancy_regex`-fallback matcher
  (`src/regexp.rs`) for lookaround/backreferences `regex` cannot express. Tests use inline
  fixtures shaped like real `mautrix-whatsapp` and legacy `mautrix-python` registration files, plus
  a double-puppet (`url: null`) fixture.
- **Store-backed registry** (`src/store.rs`, `src/registry.rs`) over `hs-tables`/`hs-kv`: add,
  list, update (RFC 7396 JSON Merge Patch), pause/resume, remove, rotate-tokens, import (idempotent
  upsert by id, for hot-reloadable static registration files), namespace conflict detection
  (sender collision, exclusive-namespace-vs-sender, identical-exclusive-pattern-twice, plus an
  `ExternalIdentityChecker` seam for track 04's future user table — see "Interfaces needed"),
  health (`healthy`/`degraded`/`down`/`paused`/`unknown` computed from consecutive failures vs.
  `hs-config`'s `tracking_failure_threshold`), and backlog listing. `as_token`/`hs_token` are
  uniquely indexed via `hs-tables`' declarative `IndexDef`.
- **Transaction scheduler** (`src/transaction.rs`, `src/scheduler.rs`): ordered per-appservice
  delivery (strict prefix, never skips a backed-off entry), batching (merges consecutive ready
  entries into one HTTP transaction via a generic JSON deep-merge), retry with exponential+jittered
  backoff, dead-letter after a configurable attempt budget, replay (by transaction id or since a
  timestamp), and full persistence in `hs-tables` (no in-memory queue at all — a restart is
  provably lossless because there was never a volatile copy; see
  `scheduler::tests::restart_resumes_pending_delivery`). Transaction bodies carry both stable and
  legacy key spellings exactly as Appendix B lists, gated per-registration-flag the way Synapse's
  `push_bulk` gates them (`refs/synapse/synapse/appservice/api.py`, read for behavior only) —
  see "Decisions made" for the one place this goes further than Synapse's current behavior. `url:
  null` registrations are enforced as never-pushed-to at the `enqueue` boundary itself (nothing is
  ever written to their queue), not by a drain-time check.
- **`hs-bridge-conformance`** (`crates/hs-bridge-conformance`; 4 scenarios, all green): a real
  HTTP fake bridge (`src/fake_bridge.rs`, bound to a loopback TCP port with `axum::serve`, not an
  in-process shortcut) plus a harness (`src/harness.rs`) wiring this crate's real `Registry`,
  `Scheduler`, `PingService` and `hs-auth`'s real `AuthState`/`Requester` machinery together.
  Scenarios (`tests/conformance.rs`): transaction contents and every documented key spelling over a
  real HTTP PUT; a null-url registration provably receiving nothing even when a reachable bridge
  exists; ping round-tripping in both directions (outbound call, and the inbound
  `/_matrix/client/v1/appservice/{id}/ping` route triggering it); identity assertion and device
  masquerading driven through `hs-auth`'s actual `Requester` extractor. See "Known gaps" below for
  what Appendix B calls for that this suite does not yet exercise, and why.
- **Ping, both directions** (`src/ping.rs`, `src/routes.rs`): outbound `POST
  {url}/_matrix/app/v1/ping` with health bookkeeping; inbound `POST
  /_matrix/client/v1/appservice/{appserviceId}/ping` as a real axum route (mounted on
  `State<hs_auth::state::AuthState>`, `PingService` passed via `Extension` — see `src/routes.rs`'s
  module docs for why), enforcing the spec's "only the named appservice's own `as_token`" rule and
  mapping `M_URL_NOT_SET`/`M_CONNECTION_TIMEOUT`/`M_BAD_STATUS`/`M_CONNECTION_FAILED` to their
  spec'd HTTP statuses (400/504/502/502), read from
  `refs/matrix-spec/data/api/client-server/appservice_ping.yaml`.
- **User/room-alias query protocol and third-party lookups** (`src/query.rs`): outbound
  `GET /users/{userId}`, `GET /rooms/{roomAlias}`, `GET /thirdparty/protocol/{protocol}`,
  `GET /thirdparty/location/{protocol}`, `GET /thirdparty/location`,
  `GET /thirdparty/user/{protocol}`, `GET /thirdparty/user` — all six calls `ruma_appservice_api`
  documents, reusing its `thirdparty::{Location, Protocol, User}` response types.
- **`AppserviceRegistry` implementation** (`src/auth_registry.rs`): replaces `hs-auth`'s stub
  (`crates/hs-auth/src/appservice.rs`) with the real registry, including the documented
  `fancy_regex`-vs-`regex::Regex` divergence (a namespace pattern needing lookaround/backreferences
  is dropped from `hs-auth`'s fast masquerade check with a `tracing::warn!`, never silently
  mis-evaluated — see that module's docs).
- `docs/rfcs/0009-appservice-identity-capability-flags.md`: the interface `hs-auth` needs to add
  (two `bool` fields) before rate-limit exemption and MSC4190 can be enforced end to end — see
  "Interfaces needed".

## In progress / Known gaps

Since 2026-09-22 the first four items below are superseded by `docs/bridges/heisenbridge.md`'s
list: the pump delivers events only (no ephemeral, to-device or device-list data yet, though
`Transaction` has the fields); one process must pump (delivery is not shard-gated for a cluster); and no mautrix-* bridge with
an external service has been tried, only heisenbridge.

Everything below is a real, specific gap, not a vague TODO — each is blocked on a concrete thing
this track does not own, listed so the next session (or another track) can pick it up precisely:

1. **MSC4190 device flow is not enforced.** `crates/hs-auth/src/routes/devices.rs`'s `put_device`
   404s on an unknown device unconditionally, and `delete_device` always requires UIA. Both need an
   `is_some_and(|a| a.msc4190_enabled)` branch, which needs the field RFC 0009 proposes. I did not
   implement this myself because it requires editing `hs-auth`'s owned routes; RFC 0009 is the
   handoff. `hs-bridge-conformance` does not yet have an MSC4190 scenario for the same reason —
   once RFC 0009 lands, add one that `PUT /devices/{id}` an unknown device as a masquerading
   appservice with `msc4190: true` and asserts creation instead of 404.
2. **Rate-limit exemption (`rate_limited: false`) is stored and round-tripped by the registry but
   not enforced.** Same root cause as (1): `AppserviceIdentity`/`AppserviceRecord` have no
   `rate_limited` field for a rate limiter to consult yet (RFC 0009). Whichever crate ends up
   owning the live rate limiter (`hs-http::ratelimit` or `hs-auth::ratelimit` — both exist today as
   separate modules; I did not referee which one is canonical, that is track 07/12's call) needs to
   check it.
3. **`ts` timestamp massaging is not exercised.** It is a room-send-time concern
   (`PUT /rooms/{room}/send/...?ts=...`, honored only for appservice-authenticated requests) that
   belongs in the room actor's send handler, which is track 04's (`hs-room`) and does not exist
   yet. The contract is simple once that handler exists: if `requester.appservice.is_some()` and a
   `ts` query parameter is present, use it as `origin_server_ts` instead of wall-clock time. No RFC
   filed since there is no code yet to interface with; flagging here so track 04 sees it when
   `hs-room` lands.
4. **MSC3983/MSC3984 key proxies are not implemented.** These need `hs-e2e` (track 08)'s key
   upload/claim storage to proxy through; `hs-e2e` is not far enough along yet for a concrete
   interface to design against. Left for a follow-up session once track 08 has landed key storage.
5. **`com.devture.shared_secret_auth` native login provider (item 6, "if budget remains") was not
   started this session** — budget went to the higher-priority items 1 through 5 in the assignment.
   It needs `hs-auth`'s login-method registry (whatever shape that takes; `hs-auth/src/routes/
   login.rs` exists but I have not surveyed its extensibility) and does not depend on anything this
   session built, so it is a clean pickup for next time.
6. **The `Bridge` custom resource, console pages, real-bridge CI, and Compose files for
   `ergo`/`mautrix-irc` and Zulip/`mautrix-zulip`** (day-one work per the brief) were not started —
   they depend on `hs-operator` (track 12) and `web/` (track 15) existing enough to integrate
   against, and on the scheduler/registry landing first, which is what this session prioritized per
   the explicit delivery order given.
7. **Running `hs-bridge-conformance` against Synapse 1.161 as a control** (the brief's day-one
   work, and this suite's own module docs) needs a running Synapse behind Docker, which is
   unavailable in this environment. The suite as written exercises this crate's own sender-side
   components (`Scheduler`, `PingService`, `Requester` wiring) against its own real HTTP fake
   bridge; it does not yet have a mode that points at an external base URL (real Synapse or a real
   `hs serve`) because no such running server exists to point it at yet either (`hs-cli` has not
   assembled a full binary). Next step once both exist: add a `SYNAPSE_BASE_URL`/`HS_BASE_URL`-gated
   variant of each scenario that drives the real client-server API to generate the traffic instead
   of calling this crate's components directly.
8. **Direct media federation (`.well-known`, key server, signed requests, multipart)**,
   **async media**, and **encrypted-bridging soak** are federation/media/e2ee dependent
   (tracks 06, 09, 08) and not started.

## Next

In priority order for a follow-up session: (1) apply RFC 0009 (or get track 07 to) and wire
MSC4190 + rate-limit exemption end to end with conformance scenarios; (2) survey `hs-auth`'s login
route extensibility and add `com.devture.shared_secret_auth`; (3) once `hs-cli` assembles a real
server binary, add the external-base-URL conformance mode and run it against Synapse in Docker as
the brief's day-one work calls for; (4) `Bridge` CRD and console pages once tracks 12/15 have
something to integrate against.

## Blockers

None blocking today's work; items above are scoped to specific other tracks landing further, not
to anything broken.

## Interfaces provided

- `hs_appservice::registry::Registry<B: hs_kv::KvBackend>`: the registration/health/backlog API —
  intended consumer: track 15's admin API and console (shapes chosen to match
  `crates/hs-admin/openapi/openapi.yaml`'s `AppService`/`AppServiceHealth`/
  `AppServiceBacklogEntry`/`AppServiceReplayRequest` schemas field-for-field), and track 12's
  `AppService`/`Bridge` operator status once it exists.
- `hs_appservice::scheduler::Scheduler<B>` and `hs_appservice::ping::PingService<B>`: what a room
  actor (track 04) and the admin API (track 15) call to enqueue events and trigger pings,
  respectively. `Scheduler::enqueue` takes a generic `transaction::Transaction` of `serde_json::
  Value` events rather than a `hs-room`/`hs-model` event type, deliberately, since those crates'
  event types do not exist in a form this crate could depend on yet — track 04 will need to
  serialize to the same shape (`events: Vec<Value>` of canonical JSON) when it lands, not adopt a
  new type.
- `hs_appservice::auth_registry::RegistryAppserviceAdapter<B>`: implements `hs_auth::appservice::
  AppserviceRegistry`, wired into `hs_auth::state::AuthState::appservices` by whichever crate
  assembles the real server (not yet assembled — `hs-cli` is a placeholder today). This replaces
  track 07's `InMemoryAppserviceRegistry` stub 1:1; no other change to `hs-auth` is needed to adopt
  it.
- `hs_appservice::routes::ping_router::<B>`: an axum `Router<hs_auth::state::AuthState>` fragment
  for `/_matrix/client/v1/appservice/{appserviceId}/ping` — mount it under
  `/_matrix/client/v1/appservice` on a router whose `State` is `AuthState`.

## Interfaces needed

- **`docs/rfcs/0009-appservice-identity-capability-flags.md`** (filed this session): `hs_auth::
  appservice::AppserviceRecord` and `hs_auth::requester::AppserviceIdentity` need `rate_limited:
  bool` and `msc4190_enabled: bool` fields, populated by `RegistryAppserviceAdapter` (already ready
  on this crate's side) and consulted by the rate limiter and `hs-auth`'s device routes
  respectively. Needs track 07 (or a follow-up from this track, with track 07's sign-off since it
  touches their crate).
- **A room actor to enqueue against** (track 04, `hs-room`): does not exist yet. `Scheduler::
  enqueue`'s shape (see above) is the contract track 04 should target.
- **`hs-e2e` key storage** (track 08): needed for MSC3983/MSC3984 key proxies.
- **`hs-user`'s user table** (track 04 or wherever it lands): `registry::ExternalIdentityChecker`
  is the seam this crate already defined for it — implement the trait and pass it to `Registry::
  with_identity_checker` once a real user table exists, to extend namespace conflict detection to
  cover human accounts (today it only catches registry-vs-registry conflicts, honestly, and the
  trait's default `NoExternalUsers` says so in its doc comment).
- **A server binary to mount everything in** (track 12's `hs-cli` today is close to a placeholder):
  needed to run `hs-bridge-conformance` against a real `hs serve` or against Synapse.

## Decisions made

- **`receive_ephemeral` and `de.sorunome.msc2409.push_ephemeral` are tracked as two independent
  booleans**, not folded into one "wants ephemeral" flag, after reading Synapse's actual loader
  (`refs/synapse/synapse/config/appservice.py`: `supports_ephemeral = as_info.get
  ("receive_ephemeral", False)` and `supports_unstable_ephemeral = as_info.get
  ("de.sorunome.msc2409.push_ephemeral", False)`, read separately) and its sender
  (`refs/synapse/synapse/appservice/api.py`'s `push_bulk`, which emits the stable `ephemeral` key
  only if the former is true and the legacy `de.sorunome.msc2409.ephemeral` key only if the latter
  is true, independently). A registration that sets only the legacy key must receive only the
  legacy key, or Appendix B's "test over real mautrix registration files" bar is not actually met —
  this was not obvious from Appendix B's prose alone (which reads like one flag) and only became
  clear from the Synapse source; documented at length in `registration.rs`'s field docs and in
  `transaction.rs`'s `to_wire_json` docs.
- **This crate's transaction sender goes one step further than Synapse's current behavior**: it
  sends both `to_device` and `de.sorunome.msc2409.to_device` (Synapse today sends only the legacy
  spelling, per a comment in its own source noting MSC4203 has not completed FCP merge), and both
  the bare stable and `org.matrix.msc3202.`-prefixed spellings of `device_lists`/one-time-key
  counts/fallback key types (Synapse sends only the prefixed spellings). This follows `PLAN.md`
  Appendix B's explicit instruction to send "both stable and legacy key spellings exactly as
  Appendix B lists" over what I could verify of Synapse's current behavior from source, since
  Appendix B is itself derived from `mautrix-go`'s parser (which already accepts both) and sending
  an extra, ignored key costs nothing. Flagged here so a differential-testing session against a
  real Synapse notices this is a deliberate, not accidental, difference.
- **A `url: null` registration is never even enqueued**, not enqueued-then-suppressed-at-drain.
  Chosen because Synapse's own interest routing (`is_interested_in_user`, checked regardless of
  exclusivity) would otherwise make a double-puppet registration's typically-broad non-exclusive
  namespace "interested" in nearly every event on the server, growing an unbounded backlog for
  something that will provably never be drained. Enforced in `Scheduler::enqueue`, documented
  there.
- **Namespace conflict detection is the decidable subset, not full regex-intersection.** Checks:
  duplicate `sender_localpart`; a new registration's exclusive namespace matching an existing
  appservice's literal sender (and vice versa); and byte-identical exclusive patterns registered
  twice. Regex-vs-regex overlap in general is undecidable and, as far as I could establish reading
  `refs/synapse/synapse/appservice/__init__.py`, Synapse does not attempt it either — this is not a
  shortcut relative to the reference implementation.
- **Appservice tokens are generated as 64 lowercase-hex characters** (32 random bytes via `rand`
  + `hex`), matching the shape `mautrix`'s own registration generators and `openssl rand -hex 32`
  (the common manual-setup instruction in bridge READMEs) both produce — deliberately not
  `hs-auth`'s `syt_`/`syr_`/`syl_` user-token shapes, since appservice tokens are a different
  credential space with different real-world tooling expectations.
- **The transaction scheduler's per-appservice concurrency model is "call `Scheduler::drain`
  again"**, not a built-in background task per appservice. `PLAN.md`'s open question ("batching
  windows and per-appservice concurrency") is answered at the batching level (a config'd
  `max_batch`) but deliberately left open at the "who calls `drain` and how often" level, since that
  decision depends on `PLAN.md` section 5.2's cluster ownership model (room-owner-driven wake-ups
  once `hs-cluster`/`hs-room` exist) which is not this track's to design.

## Shared dependencies added

- `fancy-regex = "0.19"` to `[workspace.dependencies]` (`Cargo.toml`), for namespace patterns using
  lookaround/backreferences the `regex` crate cannot express (see `src/regexp.rs`). Only this
  track's own crates currently depend on it.
- `reqwest` and `ulid` were already present in `[workspace.dependencies]` (added by track 15) and
  reused as-is for this track's outbound HTTP clients (transaction delivery, ping, queries) rather
  than hand-rolling a `hyper`/`hyper-rustls` client — noted here since it is a meaningful dependency
  choice even though the `Cargo.toml` entry itself predates this session.
- No other new `[workspace.dependencies]` entries. Per-crate dev-dependency additions only
  (`tower` in both this track's crates' `[dev-dependencies]`, for `tower::ServiceExt::oneshot` in
  axum router tests — already a workspace dependency, just not previously listed for these two
  crates).

## `/versions` `unstable_features` flags track 12 needs, per Appendix B

Every flag a mautrix bridge probes on `/versions` (`bridgev2`'s minimum accepted spec version is
v1.4; flags below are checked regardless of declared spec version, so advertise `true` the moment
the underlying feature actually works, not only once the corresponding spec version is declared):

| Flag | What it gates | Owner of the underlying feature | Advertise once |
|---|---|---|---|
| `fi.mau.msc2246.stable` | Async media uploads | track 09 (`hs-media`) | async upload endpoints (`POST /media/v1/create`, `PUT /media/v3/upload/{server}/{id}`) exist |
| `fi.mau.msc2659.stable` | Appservice ping | **this track** | `hs_appservice::routes::ping_router` is mounted on the real server (component done; mounting is not) |
| `org.matrix.msc3916.stable` | Authenticated media | track 09 | authenticated `/_matrix/client/v1/media/*` endpoints exist |
| `uk.half-shot.msc2666.query_mutual_rooms` and `.stable` | Mutual rooms | track 04/05 (whichever owns `/v1/mutual_rooms`) | that endpoint exists |
| `org.matrix.msc4194` | User redaction | likely track 03/04 (room actor) | `rooms/{room}/redact/user/{user}` exists |
| `fi.mau.msc2815` | View redacted content | track 04 | implemented |
| `uk.timedout.msc4323` and `.stable` | Account moderation | track 04/13 (admin) | `admin/{action}/{target}` exists |
| `uk.tcpip.msc4133` and `.stable` | Extended profiles | track 04 (`hs-user`) or 07 | `profile/{id}/{key}` exists |
| `org.matrix.msc4143` and `.stable` | MatrixRTC | not this track's; primarily Element Call's, but `bridgev2` probes it too | RTC transports endpoint exists |
| `com.beeper.msc4169`, `com.beeper.msc4437`/`.stable`, `com.beeper.msc4446`, `com.beeper.hungry`, `batch_sending`, `room_yeeting`, `room_create_autojoin_invites`, `arbitrary_profile_meta`, `account_data_mute`, `inbox_state`, `arbitrary_member_change` | Beeper-only extensions | — | **never** — `PLAN.md` section 8.1 point 6 and Appendix B both say these are intentionally not implemented, matching Synapse's own non-advertisement |

`GET /_matrix/client/versions` itself does not exist yet (track 12 is adding it per this track's
brief); this table is the appservice-relevant subset of its `unstable_features` map to drive from
config, per the assignment's request.
