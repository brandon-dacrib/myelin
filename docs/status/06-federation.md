# 06 Federation: status

> **Integration note, 2026-09-19 (integration lead):** the gap this file describes below as "the
> one gap this session could not close" — no way to persist a newly received foreign event — was
> **closed** by track 04's `hs_room::actor::RoomActor::accept_remote_event`. This (fifth) session
> closed the next one: the backfill-then-retry loop `MissingAncestors` was reported for but never
> consumed. See "Fifth session: the backfill loop" below.

Updated: 2026-09-19 (fifth session -- the backfill session: a remote event citing ancestors this
server lacks now gets them fetched, verified and persisted, then the original event is retried; see
"Fifth session: the backfill loop" below). Previously updated 2026-09-18 (fourth session -- the
write session: `/send` and the join handshake are real now, not seams; see "Fourth session: `/send`,
`make_join`/`send_join`, and the v2 mount fix" below). Before that, 2026-09-18 (third session, the
mounting session; see "Mounted into `hs serve`" below for what changed then). The first session
wrote the threat model and the plan below but stopped before any crate code existed; the second
implemented items 1-7 of that plan.
`crates/hs-federation` is no longer a placeholder: 119 passing lib tests (up from 114), plus 7 + 8
real end-to-end tests in `hs-cli` driving a running composed router with genuine signed requests,
`cargo clippy -p hs-federation --all-targets -- -D warnings` clean, five fuzz targets that
type-check. Read this file before touching `hs-federation` further.

## Fifth session: the backfill loop

Scope (`docs/next-steps.md` item 4): consume `RoomError::MissingAncestors` instead of merely
reporting it. A remote's join could already be persisted (fourth session); the very next event
that server sent citing history from before the join could not, because nothing fetched that
history. This session closes that.

### 1. The loop, in one paragraph

New module `crates/hs-federation/src/backfill.rs`. When `RoomWriteSink::accept_verified_event`
rejects an event because it names ancestors this server does not hold
(`WriteRejected::missing_ancestors`, new -- see below), `crate::inbound::process_transaction`
calls `crate::backfill::resolve_missing_ancestors(origin, room_id, room_version, missing_ids,
fetcher, key_cache, sink, limits)`. That function asks `origin` (the server that sent the
transaction -- the natural peer to ask, since it is the one that told us about an event referencing
history it presumably has) for the missing events via `GET /backfill/{roomId}?v=...&limit=...`,
verifies each returned PDU exactly the way `verify_pdu` verifies any inbound PDU (content hash,
then signature against the *sender's* server, not `origin`), and persists them through the same
`RoomWriteSink` in dependency order. If persisting one of *those* events itself reports a deeper
`MissingAncestors` (the gap is more than one event deep), that becomes the next round's fetch
target -- the loop is genuinely recursive, not a single fetch-and-hope. Once the fetched events are
all either persisted or hard-rejected (bad auth -- see below), control returns to
`process_transaction`, which retries the original event exactly once. Success or failure of that
retry is reported the normal way: a per-event `{}` or `{"error": ...}` inside the transaction
response, never a fatal transaction failure -- backfill giving up looks, from `/send`'s caller's
point of view, exactly like the event being unresolvable for any other reason.

### 2. The limits, and why

`BackfillLimits` (`crate::backfill`), four independent dimensions, all required to fail before the
attempt gives up on a genuine gap it just cannot close, and any one of which stops the attempt on
its own:

| Field | Default | What it bounds |
|---|---|---|
| `max_events_per_fetch` | 100 | The `limit` sent on each `/backfill` request, **and** the most events accepted from one response even if the peer sends more. Matches `transport::read_routes::MAX_BACKFILL_LIMIT`, this server's own server-side clamp on the same endpoint -- this server never asks for more than it would itself agree to answer, and never trusts a peer that ignores the `limit` it was given. |
| `max_rounds` | 10 | The most `/backfill` round-trips one resolution attempt makes to the same peer. This is the recursion-depth bound: each round can surface a new, deeper gap (a fetched event's own `prev_events` can themselves be missing), so a chain deeper than 10 hops is given up on, not chased further. **Confirmed load-bearing by mutation test** -- see below. |
| `max_total_events` | 500 | The total number of events fetched and signature-*verified* (an asymmetric-crypto operation) across every round, independent of how few rounds it took to reach that count. This is the real cost-of-attack bound: rounds alone would not stop a peer that returns many events per round. |
| `max_duration` | 20s | Wall-clock ceiling for the whole attempt (`tokio::time::timeout` around the entire resolution), so a slow-but-not-failing peer cannot hold the task open indefinitely. |

The request itself is also bounded independent of the response: `frontier.iter().take
(max_events_per_fetch)` caps how many event IDs go into one `?v=...&v=...` query string, so a
round that somehow discovered many independent gaps at once cannot turn into an unbounded URL.

A peer that returns *nothing new* (an empty response, or a response containing only events already
seen earlier in the same attempt) is treated as "cannot make progress" and the attempt gives up
immediately (`BackfillGiveUpReason::StillMissing`) rather than retrying the same request pointlessly
for the remaining rounds -- a real inability to help is distinguished from "still trying".

### 3. What an attacker can and cannot cost this server

**Can**: force up to 10 HTTP round-trips to itself, up to 500 signature verifications (Ed25519
verify is cheap -- microseconds -- and `RemoteKeyCache` already deduplicates concurrent lookups for
the same `(server, key_id)`, so the realistic cost is closer to "500 verifies against a handful of
cached keys" than 500 separate key fetches), and up to 20 seconds of one Tokio task's wall-clock
time, **per event that names a missing ancestor**. It can repeat this for every hostile event it
sends, so the aggregate cost across many transactions is not bounded by this module alone --
but each individual attempt is small, finite, and cannot compound into recursion, an unbounded
response, or an indefinite hang. The existing per-destination concurrency limit
(`FederationClient`, default 1 in-flight request) and `DestinationStore` backoff additionally throttle
*how fast* a single hostile server can trigger repeated attempts, though that is a pre-existing
defence this session did not add, not something `backfill.rs` itself enforces.
**Cannot**: make this server recurse forever (bounded by `max_rounds` and `max_total_events`,
enforced independently so neither alone is a single point of failure), make it accept an
unauthorized or malformed event (every fetched event still goes through the exact same
`verify_pdu` + `RoomActor::accept_remote_event`'s two-snapshot authorization check as any other
inbound event -- backfill is a *source* of candidate events, not a bypass of anything that checks
them), make it hang past 20 seconds on one attempt, or make it send an unbounded request (the `v=`
list is capped the same as the response).

### 4. The outbound `/backfill` client

`FederationClient::backfill` (`crates/hs-federation/src/client.rs`), alongside the inbound
`/backfill` server (`transport::read_routes::backfill`, existing since the second session). Builds
`GET /_matrix/federation/v1/backfill/{roomId}?limit=N&v=...`, calls the existing
`FederationClient::send` (the one signed-request path every other outbound call already goes
through -- no second X-Matrix client), and returns the raw, **unverified** `pdus` array. Verifying
is deliberately not this method's job: `crate::backfill::resolve_missing_ancestors` is the one
place that owns "fetch, then verify" as a sequence, so there is exactly one path where a fetched
event might be trusted before it is checked, and it is easy to audit.

`crate::backfill::AncestorFetcher` is the seam `FederationClient` implements this through
(`impl AncestorFetcher for FederationClient`), the same shape as `RoomDataSource`/`RoomWriteSink`:
a trait this crate owns, so `crate::backfill`'s own tests can supply fakes (`QueuedFetcher`,
`EndlessFetcher`) without a real HTTP server, and `hs-cli`'s integration tests can supply a real
`FederationClient` pointed at a real loopback listener.

### 5. Mutation test performed this session

Per this session's instructions: `BackfillLimits::default().max_rounds` was changed from `10` to
`usize::MAX` in `crates/hs-federation/src/backfill.rs`, and
`backfill::tests::gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever` (which asserts
the endless-chain peer above is given up on after exactly 10 round-trips with
`BackfillGiveUpReason::TooManyRounds`) was re-run:

```
thread 'backfill::tests::gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever' panicked
  at crates/hs-federation/src/backfill.rs:604:9:
Err(TooManyEvents)
test result: FAILED. 0 passed; 1 failed; ... finished in 3.18s
```

The test failed exactly as expected: with the round bound removed, the *other* independent limit
(`max_total_events`, still 500) caught the runaway instead, 500 rounds in instead of 10 -- proving
`max_rounds` is what the original test's "exactly 10 round-trips" assertion depends on, not
incidental behaviour elsewhere in the loop. It did **not** hang (3.18 seconds for 500 in-process
mock round-trips, no real network involved in this unit test), which is itself informative: even
with one limit disabled, the layered design meant this session never had to interrupt a genuinely
runaway process to observe the failure. The change was reverted immediately
(`max_rounds` back to `10`); `cargo test -p hs-federation --lib backfill` is green (6/6) with the
revert in place. This is recorded here rather than kept as a second standing test, because a test
cannot mutate the default it is itself asserting against without either duplicating the limit or
ceasing to test what its name says (see the comment left in place of the test in
`crates/hs-federation/src/backfill.rs`).

### 6. `WriteRejected` gained a structured `missing_ancestors: Vec<String>` field

(`crates/hs-federation/src/inbound.rs`, plus its two constructors `WriteRejected::other` and
`WriteRejected::missing_ancestors`.) Previously the only signal was a human-readable `error`
string; `hs_cli::federation::RegistryWriteSink` had already started interpolating the missing IDs
into that string for operator-log readability, but nothing machine-readable distinguished "missing
ancestors, maybe backfillable" from "any other rejection, not backfillable" without string-matching
the message. `crate::backfill::resolve_inner` reads `rejected.missing_ancestors` directly.
`RegistryWriteSink::accept_verified_event` (`crates/hs-cli/src/federation.rs`) now constructs
`WriteRejected::missing_ancestors(id_strings, message)` for exactly the `RoomError::MissingAncestors`
case and `WriteRejected::other(...)` everywhere else, so the string in `error` and the structured
list can never drift apart (one format call builds both).

### 7. Testing shape

- **`crates/hs-federation/src/backfill.rs`** (6 new lib tests): `resolves_a_single_hop_gap`,
  `resolves_a_multi_hop_gap_across_several_rounds` (a two-deep chain, fetched one hop per round,
  proving the loop actually recurses and that an event fetched in an earlier round is retried once
  its own blocker lands -- this is what caught a real bug during development, see below),
  `gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever`,
  `gives_up_when_the_remote_returns_nothing`. Fakes: `QueuedFetcher` (hands back a scripted
  sequence of responses), `EndlessFetcher` (never converges), `DagSink` (a minimal
  `RoomWriteSink` that actually enforces "prev_events must already be known", closely mirroring
  `RoomActor::accept_remote_event`'s real shape without needing a real room actor).
- **`crates/hs-federation/src/client.rs`** (1 new test): `backfill_sends_a_signed_get_and_parses_the_pdus`,
  against `hs_testkit::FakeFederationPeer` over a real loopback socket -- asserts the exact request
  shape (`GET .../backfill/{roomId}?limit=N&v=...`) a real peer would receive, not just that the
  method compiles.
- **`crates/hs-cli/tests/federation_writes.rs`** (2 new end-to-end tests, `Harness` extended with
  `Harness::with_backfill_peer(port)`, a real `FederationClient` pointed at a real loopback
  listener via the explicit-port destination form `localhost:{port}` -- exactly the seam
  `crates/hs-federation/src/client.rs`'s own tests already use, so this is not a new test pattern):
  - `send_backfills_a_missing_ancestor_then_accepts_the_original_event`: a hand-built, correctly
    signed and hashed `m2` cites a hand-built `m1` this server was never sent. A minimal axum
    server (not `hs-testkit`, which `hs-cli` does not depend on -- see "Decisions made") answers
    `/backfill` with exactly `m1`. Asserts `m2` is accepted and `m1` is independently readable back
    through `/event/{id}` afterwards -- both fetched-via-backfill and original-event persistence
    are checked, not just "the transaction returned 200".
  - `send_gives_up_when_the_remote_serves_an_endless_backfill_chain`: the hostile peer described
    above, over real HTTP. Asserts the transaction reports a per-event error mentioning the
    give-up (not a hang, not a fake success) and that **exactly** `BackfillLimits::default()
    .max_rounds` requests reached the peer -- the bound is checked by counting real HTTP requests
    that arrived, not just by inspecting the returned error type.

**A real bug this session's own tests caught before it reached these two integration tests**: the
first version of `resolve_inner` dropped a fetched-but-blocked event the moment its first
persistence attempt failed, re-deriving only the next `frontier` from it and discarding the event
itself. That worked for a single-hop gap but silently lost multi-hop chains: fetching `e1` (which
unblocks `e2`, already fetched and discarded in the previous round) would never retry `e2`, and the
loop would report success once `e1` alone persisted even though `e2` -- the actual descendant
needed -- was still missing. `backfill::tests::resolves_a_multi_hop_gap_across_several_rounds`
failed immediately (`assertion failed: sink.known.lock().unwrap().contains(&e2_id)`) and pinpointed
exactly this. Fixed by carrying a `pending: Vec<Event>` worklist across rounds (not just within one)
and re-attempting the *entire* worklist, sorted ancestors-first by `depth`, every round -- so an
event unblocked by this round's fetch is retried in the same pass that unblocks it, not abandoned
after its first failed attempt.

**A second real thing this session's tests caught, about the auth rules rather than backfill
itself**: constructing a hand-signed test event first failed with "no m.room.create event in auth
events" -- this session had assumed (incorrectly) that room version 11 excludes `m.room.create`
from a message event's `auth_events` selection the way version 12 does
(`room_create_event_id_as_room_id`). Reading `crates/hs-model/src/room_version.rs` directly showed
V11 does **not** set that flag (only V12 does); `hs_state::auth::expected_auth_types` therefore
still requires `m.room.create` in the selection for V11. Not a bug in this session's production
code -- a wrong assumption in the test's construction, caught by the real auth rules doing their
job. Recorded here because the next person hand-constructing a V11 test event will hit the exact
same thing.

### 8. Wiring

`hs_cli::federation::build_mount` (`crates/hs-cli/src/federation.rs`, this track's file) now sets
`ancestor_fetcher: Some(client.clone() as Arc<dyn hs_federation::backfill::AncestorFetcher>)` --
the *same* `FederationClient` every other outbound call already uses, so backfill shares its
signing key, per-destination concurrency limit and backoff state with everything else this server
sends. `manifest_only_mount` sets `ancestor_fetcher: None` (routes-manifest generation needs no
outbound capability). No `serve.rs` change was needed this session -- `FederationState` already
flowed through unchanged mount points; the two new fields are just two more fields on a struct that
was already being threaded through.

### Verification

```
cargo fmt -p hs-federation -p hs-cli                              # applied, no diffs after
cargo clippy -p hs-federation --all-targets -- -D warnings        # clean
cargo clippy -p hs-cli --all-targets --no-deps -- -D warnings     # clean (see fourth session's
                                                                   # note on why --no-deps)
cargo test -p hs-federation                                       # 119 passed (lib), 0 failed
cargo test -p hs-cli --test federation_reads --test federation_writes
                                                                   # 7 + 8 passed, 0 failed
```

## Done

All against `docs/design/06-federation-threat-model.md`'s catalogue; file paths below are all
under `crates/hs-federation/src/` unless stated otherwise.

- **`Cargo.toml`** (crate and workspace root). Added to `[workspace.dependencies]`:
  `hickory-resolver` (0.26, `tokio` feature — `system-config` comes along via its own default
  features) for DNS, and `ipnet` (2.x) for CIDR containment checks (promoted from a transitive dep
  of `hickory-net` to a direct one rather than hand-rolling CIDR matching). Both noted in the root
  `Cargo.toml` with "Added by track 06" comments per convention. The crate's own `Cargo.toml` pulls
  in `hs-model`, `hs-kv`, `hs-tables`, `hs-http`, `hs-config`, `ruma`, `ed25519-dalek`, `rand_core`,
  `axum`, `tokio`, `reqwest`, `regex`, `async-trait`, and dev-deps `hs-testkit`, `tempfile`,
  `tower`, `rcgen`.
- **`discovery.rs`** (20 tests). The full server-name resolution algorithm (IP literal / explicit
  port / well-known+SRV+fallback), behind three traits (`WellKnownFetcher`, `SrvResolver`,
  `AddrResolver`) so the resolution logic is unit-tested against fakes reproducing the spec's
  worked examples, with real `reqwest`-backed (`HttpWellKnownFetcher`) and
  `hickory-resolver`-backed (`HickoryResolver`) implementations for production. Covers: an explicit
  port bypassing discovery entirely, a well-known that itself needs SRV resolution, SRV falling
  back to the deprecated `_matrix._tcp` service, malformed/absent well-known falling back to SRV on
  the *original* host, no-redirects-followed (structurally impossible in this design — the fetcher
  trait has no redirect-following code path at all), body-size cap enforced without buffering past
  it, and `CachingWellKnownFetcher` (a TTL decorator honouring `clamp_cache_control`, with separate,
  shorter negative caching for failures — the plan's "wrap `WellKnownFetcher` in a decorator"
  item). `MIN`/`MAX`/`DEFAULT`/`FAILED` cache TTL constants are committed to concrete values (60s /
  24h / 24h / 60s) since the first session left them unpicked.
- **`keys.rs`** (14 tests). `OwnSigningKeys::load_or_generate` (Synapse's `algorithm version
  base64_seed` line format, first-boot generation, multiple keys from separate files for rotation,
  malformed lines skipped not fatal). `build_server_key_response` (self-signed with every active
  key, `old_verify_keys` carried through). `wrap_for_notary` (adds the notary's own signature
  without touching the origin's). `RemoteKeyCache` with `KeyServerFetcher` as the fetch seam,
  per-origin in-flight de-duplication (`tokio::sync::Mutex` keyed by origin, tested with 8
  concurrent callers producing exactly 1 fetch), self-signature verification before trusting
  anything (tampered key, wrong claimed `server_name`, both rejected), and the old-verify-keys
  rule: `get_current` (now-valid only) vs `get_valid_at(ts)` (accepts a key that was valid *when
  signed* even if since rotated out, rejects one that had already expired) — this is the
  "expired key rejected, key valid at signing time still accepted" pair from the assignment,
  tested directly. Cross-server key substitution is structurally impossible (cache keyed by
  `(server_name, key_id)`, a response can only populate the server name it claims and self-signed
  for) and tested.
- **`xmatrix.rs`** (12 tests). Header parse (quoted/bare values, any field order, RFC 7235 escapes,
  rejects multiple `Authorization` headers, rejects wrong scheme, rejects a missing field) and
  build. The exact signed-object shape (`method`, `uri`, `origin`, `destination`, `content?`) built
  on `hs_model::canonical`/`signing` directly, not reimplemented. `verify_x_matrix`, the axum
  middleware: destination-must-equal-own-name, body-size cap enforced before signature
  verification reads it (`axum::body::to_bytes` bounded at 50 MiB), key lookup through
  `RemoteKeyCache::get_current`, full signature reconstruction and verification. Tests: tampered
  body rejected, wrong destination rejected, wrong key rejected, missing signature rejected,
  replayed valid signature against a *different* route rejected (proves `uri` binding actually
  works, not just "any valid sig passes"), multiple `Authorization` headers rejected, unknown
  origin rejected without ever caching a bogus key. **Found and fixed a real layering-order bug
  while writing these tests**: `Router::layer` wraps outside-in (last `.layer()` call runs
  first), so `Extension(ctx)` must be added *after* `from_fn(verify_x_matrix)`, not before, or the
  context is missing and every request 500s. Documented prominently in both the module doc and the
  `verify_x_matrix` doc comment so `crate::transport::router` (which has the same ordering
  requirement) doesn't regress it.
- **`acl.rs`** (11 tests). One `is_allowed(server_name, &ServerAcl) -> bool` function, used for
  both directions per the threat model's explicit requirement (no direction parameter exists, so
  there is no way to call it asymmetrically). Anchored glob-to-regex translation, tested for both
  match and adjacent-non-match (`*.example.org` vs `example.org` itself and vs
  `example.org.evil.com`; `?` matching exactly one character, not zero or two; a literal `.` in a
  pattern not acting as regex "any character"). `allow_ip_literals` checked against the *string
  form* of the server name, distinct from (and not a replacement for) `ip_range_blocklist`.
- **`room_source.rs`** (4 tests). The `RoomDataSource` trait per threat model section 5's seam
  decision (membership/visibility, event/state/state-ids/auth-chain/backfill/missing-events/
  hierarchy/timestamp-lookup, plus `get_event_by_id` for `/event/{eventId}`'s room-less path
  shape), and `InMemoryRoomSource`/`FakeRoom` for this crate's own tests. Every method takes
  `requesting_server` explicitly; `InMemoryRoomSource` enforces the visibility check *before*
  returning content in every method, and a test asserts a non-member gets `NotVisible` before ever
  seeing event content.
- **`destination_store.rs`** (7 tests). `DestinationState` (failure count, next retry time,
  exponential backoff base 1s doubling capped at `max_backoff_ms`, full jitter). `DestinationStore`
  trait with `InMemoryDestinationStore` and `KvDestinationStore<B: KvBackend>` (a real
  `hs_tables::TypedKeyspace` under `hs_federation.destinations`). The "persisted so a restart
  resumes" requirement is tested directly: two independent `KvDestinationStore` handles opened over
  the *same* `MemoryBackend` instance (simulating a process restart against the same on-disk store)
  see the same accumulated failure count.
- **`client.rs`** (10 tests). `FederationClient`: per-destination `tokio::sync::Semaphore`
  (default concurrency 1, tested that two concurrent sends to the same destination both complete
  rather than deadlocking), `DomainPolicy` (checked against the *original* server name, before any
  discovery happens — tested that a denied domain never reaches the network), `IpPolicy` (CIDR
  block/allow via `ipnet`, allowlist overrides blocklist, checked against the *resolved* address —
  tested that a blocked destination is rejected even though `AddrResolver` successfully resolved
  it), backoff-aware dispatch via `DestinationStore` (a backing-off destination is rejected before
  any network call), and a `reqwest::Client` cache pinned per destination via `.resolve()` so the
  TLS SNI / `Host` stays the delegated name while the TCP connection goes to the actually-resolved
  address (the spec's delegation contract). HTTP/1.1-only to peers, per the brief's recommendation
  — committed as a decision below. Tested end-to-end against `hs_testkit::FakeFederationPeer` bound
  to a real loopback socket (see `ClientConfig::scheme`, a documented test-only seam — see
  "Decisions made").
- **`transport/`** (a `mod.rs` + `read_routes.rs` + `seams.rs` + `queries.rs`, 9 tests in `mod.rs`
  + `read_routes.rs` + `seams.rs` combined). `router()` builds one merged `axum::Router` from
  `read_routes::add_routes` and `seams::add_routes` composed into a single `hs_http::router::Builder`
  (not two separately-built routers merged at the root — axum forbids nesting at `""`, discovered
  while wiring this up), then applies `axum::middleware::from_fn(verify_x_matrix)` and
  `Extension(x_matrix_ctx)` **exactly once**, over the whole thing, in the correct order (see the
  xmatrix.rs bug above). **The load-bearing test**
  (`transport::tests::every_route_is_behind_the_x_matrix_layer`) walks the router's own
  `RouteManifest` — not a hand-maintained list — substitutes a placeholder for every `{param}`
  path segment, and asserts every single registered route returns 401 with no `Authorization`
  header. This test does not need updating when a route is added; it fails automatically the
  moment a future route ships outside the merge. Fully implemented, against `RoomDataSource` and a
  small `FederationQuerySource` seam (profile/directory/devices/openid — not room-scoped, so not
  part of `RoomDataSource`): `/version`, `/query/{queryType}` (profile, directory),
  `/user/devices/{userId}` (gated on `allow_device_name_lookup_over_federation`), `/publicRooms`
  (gated on `allow_public_rooms_over_federation`, limit clamped to 100), `/hierarchy/{roomId}`,
  `/timestamp_to_event/{roomId}`, `/openid/userinfo`, `/event/{eventId}`, `/state/{roomId}`,
  `/state_ids/{roomId}`, `/event_auth/{roomId}/{eventId}`, `/backfill/{roomId}` (limit clamped to
  100), `/get_missing_events/{roomId}` (limit clamped to 100). Every per-room handler checks
  membership/visibility via `RoomDataSource` before returning content — tested directly (a
  non-member gets 403, a member gets 200, for the same event). `/send`, every join/leave/knock/
  invite handshake variant (v1 and v2 where the spec has both), `/exchange_third_party_invite`,
  `/3pid/onbind`, `/user/keys/claim`, `/user/keys/query` (joint-owned with track 08), and
  `/rooms/{roomId}/complexity`, `/extremities/{roomId}`, `/query/account_status` are mounted as
  seams: one shared `not_implemented` handler, behind the same layer as everything else, returning
  a typed 501. A test walks every seam route the same way the layer test does and asserts every one
  responds 501. Media endpoints and the two `_synapse/client/*` compat entries are deliberately
  **not** mounted (media is track 09's; `_synapse/client/*` is client-prefixed, not
  server-to-server).
- **`edu.rs`** (6 tests, new — not in the original plan's enumerated file list, but needed to give
  the required "EDU JSON" fuzz target something real to call). `parse_edu`: structural validation
  only (`edu_type` string required, `content` object-or-absent, size-capped at the same 64 KiB
  PDUs get, canonical-JSON validated via `hs_model::canonical::to_canonical_object` so a malformed
  number is rejected the same way it would be for a PDU). Not wired into a handler yet (`/send`
  is still a seam) — this is deliberately just the parser, matching the instruction that a parser
  touching remote input needs a fuzz target regardless of whether its caller exists yet (see
  `crates/hs-media/fuzz`'s own precedent, cited in the brief, for exactly this pattern).
- **Fuzz targets** (`crates/hs-federation/fuzz/`, five targets, one seed pair each). `pdu_parse`
  (drives `hs_model::Event::parse` directly, reusing track 02's parser, not a new one).
  `edu_parse` (drives `crate::edu::parse_edu`). `xmatrix_header_parse` (drives
  `crate::xmatrix::parse_x_matrix_header` via a fuzzed `HeaderValue`). `well_known_body_parse`
  (drives a newly-extracted `crate::discovery::parse_well_known_body`, now also used by the real
  `HttpWellKnownFetcher` so there is exactly one parsing path, not two that could drift).
  `key_server_response_parse` (drives `RemoteKeyCache::ingest_response`, made `pub` specifically so
  it is fuzzable without a live fetcher). **Confirmed these type-check on the stable toolchain
  installed here** (`cargo check` inside `crates/hs-federation/fuzz`, a separate `[workspace]` per
  the `hs-media/fuzz` template, succeeds) — actually *running* them needs `cargo-fuzz` and a
  nightly toolchain per the brief, neither available in this sandbox, so they have not been
  executed, only type-checked and given one valid + one adversarial seed file each under
  `fuzz/corpus/<target>/`.

## Mounted into `hs serve` (third session)

The transport server is no longer written-but-unserved. `hs serve` mounts it, and it answers from
this server's real data.

- **`crates/hs-cli/src/federation.rs`** (new, integration lead). `RegistryRoomSource` implements
  [`RoomDataSource`] over `hs_room::registry::RoomRegistry` and `hs-user`'s published-room
  directory; `ServerQuerySource` implements `FederationQuerySource` over `hs-auth`'s user/device
  store, `hs-e2e`'s device keys and `hs-room`'s alias keyspace; `ClientKeyFetcher` implements
  `KeyServerFetcher` over the real `FederationClient`, which is what makes inbound verification
  able to fetch a stranger's keys. `build_mount` assembles all of it, and takes the signing key
  from the *same* `HomeserverIdentity` `hs-room` signs events with, so the key this server
  advertises and the key it signs with cannot drift apart.
- **`crates/hs-cli/src/serve.rs`**. Mounts the transport router at `/_matrix/federation/v1` when
  `federation.enabled`, plus `GET /_matrix/key/v2/server` *outside* the `X-Matrix` layer (it is
  the one federation endpoint that must answer an unsigned request -- it is how a remote gets the
  keys it would need to sign one).
- **Two real bugs this surfaced**, both only visible once the router was mounted the way a
  deployment mounts it:
  1. **`xmatrix.rs`: the verifier signed over the wrong URI under a prefix mount.**
     `axum::Router::nest` rewrites `req.uri()` to the path *relative to* the nest prefix before
     inner layers run, so the layer was verifying against `/version` while every real sender signs
     `/_matrix/federation/v1/version`. Every inbound request from every real homeserver would have
     failed verification. Fixed by preferring the `OriginalUri` extension (`signed_uri`).
     `crates/hs-cli/tests/federation_reads.rs` mounts the router under the real prefix precisely
     so this stays caught.
  2. **`read_routes.rs`: `/backfill?v=$a&v=$b` was rejected with `400`.** `axum::extract::Query`
     deserializes through `serde_urlencoded`, which cannot build a sequence from repeated keys, so
     a `Vec<String>` field failed the whole extraction rather than collecting. Now parsed from raw
     pairs.
- **Two spec deviations fixed**: `/3pid/onbind` was registered `POST` where the spec says `PUT`,
  and `/query/profile` and `/query/directory` are now registered as their own paths (the spec
  names them; the generic `/query/{queryType}` still works).

### What the adapter deliberately will not answer

- **`/state` and `/state_ids` answer only for the room's newest event**, and return `404` for any
  older one. The room actor holds one flat current-state map and has no state-at-an-event query,
  so the newest event is the only one whose state it can report correctly. Answering a question
  about the past with the present would give a remote state it cannot tell is wrong. Lifting this
  needs `hs-state`'s historical snapshots.
- **`/openid/userinfo` resolves nothing**: no OpenID token is ever issued, because the
  client-server `POST /user/{userId}/openid/request_token` endpoint does not exist yet.
- **`/query/profile` returns an empty profile for a local user that exists**, because no profile
  storage exists anywhere in the workspace yet (there is no client-server `/profile` route
  either). It is `404` only for a user this server does not have.
- Auth chains are walked transitively from stored `auth_events` (bounded at 2,000 events), not
  read from `hs-state`'s chain-cover index, which the room actor does not maintain yet.

### Still unmounted or still wrong (as of the third session; see the fourth session below for
what changed)

- ~~The v2 join/leave/invite paths are registered under v1~~ -- fixed the fourth session, see below.
- ~~`GET /.well-known/matrix/server` is not served.~~ -- served as of this session's manifest (added
  by another track's work landing between sessions; not this track's change, noted here only so
  the "still wrong" list stays accurate).
- ~~`/send` and the join handshakes are still seams.~~ -- `/send`, `make_join` and `send_join` (v1
  and v2) are real as of the fourth session; `send_leave` and `invite` remain seams (out of this
  session's scope) but are now mounted at the *correct* v2 path. See below.

## Fourth session: `/send`, `make_join`/`send_join`, and the v2 mount fix

Scope for this session (`docs/next-steps.md` item 3): close the two federation writes that matter
most so this server can be joined by a remote and can receive its events, and fix the v2 routing
bug first. Every file below is new or changed under `crates/hs-federation/src/` and
`crates/hs-cli/src/federation.rs` / `crates/hs-cli/tests/`.

### 1. The v2 mount bug, fixed

`crates/hs-federation/src/transport/mod.rs` now exports two router functions:

- **`router(state, x_matrix_ctx)`** -- the v1 router: every read/query endpoint, every remaining
  seam, plus the newly-real `/send`, `make_join` and the v1 `send_join` spelling. Unchanged mount
  point (`/_matrix/federation/v1`).
- **`router_v2(state, x_matrix_ctx)`** -- a **new**, separate router for the v2-only spellings:
  the newly-real v2 `send_join`, plus the still-seam v2 `send_leave` and `invite`. Meant to be
  mounted at `/_matrix/federation/v2` -- a genuinely different mount point, not a `/v2/` path
  segment appended to the v1 router. This is what fixes the bug: v1 and v2 `send_join` share the
  *exact same route string* (`/send_join/{roomId}/{eventId}`) and only the mount prefix
  distinguishes them, so they cannot both be registered in one `Builder` (it would be registering
  the same `(method, path)` twice). `crates/hs-federation/src/transport/seams.rs` gained a
  matching `add_routes_v2` for the two seams that still need it.
- Both routers apply the same `X-Matrix` layer the same way (factored into a shared
  `apply_x_matrix_layer` helper); `transport::seams::tests::every_v2_seam_route_responds_not_implemented`
  and `transport::seams::tests::the_real_write_routes_are_not_registered_as_seams_here` guard both
  halves of this (the second one fails the moment `/send`/`make_join`/`send_join` are accidentally
  re-added as seams here, which is exactly the kind of regression this fix could otherwise invite).

**`hs serve` does not mount `router_v2` yet** -- I do not own `crates/hs-cli/src/serve.rs` this
session. See "Wiring the integration lead must add" below for the exact lines. Until that lands,
`docs/status/routes.json`/the coverage dashboard will keep showing only the v1 spellings; that is
this gap, not a regression.

### 2. `PUT /send/{txnId}`: real, with one honest, load-bearing gap

New module `crates/hs-federation/src/inbound.rs`:

- **`verify_pdu(raw, room_version, key_cache)`**: parses the PDU (`hs_model::Event::parse`),
  recomputes and checks its content hash against its declared `hashes.sha256` (an `Event::parse`
  does *not* do this -- confirmed by reading `hs-model`'s own doc comment on `parse`, which says so
  explicitly), then verifies its signature against the **sender's** server (not the transmitting
  `origin` -- a resident server relays other domains' events, so checking against `origin` would be
  wrong) using `hs_federation::keys::RemoteKeyCache::get_valid_at` (the existing "valid at the time
  it was signed" lookup, not `get_current`, which is what makes an old-but-was-valid key still
  verify).
- **`process_transaction(origin, txn_id, body, rooms, sink, key_cache, transactions)`**: the real
  `/send` algorithm -- checks `(origin, txn_id)` against the idempotency cache first (returns the
  cached response unprocessed if found), rejects the whole transaction with `400` if `pdus.len() >
  50` or `edus.len() > 100` (the spec's resource-limits table), otherwise processes every PDU **in
  order**, building the `{"pdus": {"$id": {}|{"error": "..."}}}` result map, then caches the
  response before returning it. EDUs are parsed for structural validity
  (`crate::edu::parse_edu`, already existed) and otherwise ignored -- no EDU handler exists yet.
- **`RoomWriteSink`** (the seam this session had to invent): `accept_verified_event(room_id,
  event_id, event_json) -> Result<WriteOutcome, WriteRejected>`. This is where the wall is -- see
  "The one gap this session could not close" below.
- **`TransactionStore`** (`get`/`put` by `(origin, txn_id)`), with an `InMemoryTransactionStore`.
  Deliberately in-memory only, not `KvBackend`-backed: the realistic threat this defends
  (a network-level retry of a transaction whose response was lost) does not survive a process
  restart anyway on the sending side either, and a `KvDestinationStore`-shaped persistent version is
  a mechanical follow-up, not a design question -- noted under "Next" rather than built this session
  to leave budget for the join handshake.
- Wired into the transport layer at `crates/hs-federation/src/transport/send.rs` (the axum handler:
  extracts the `X-Matrix`-verified origin, parses the body, calls `process_transaction`, maps
  `TransactionError` to `400`).

### 3. `make_join` / `send_join` (v1 and v2)

New module `crates/hs-federation/src/join.rs`, wired at
`crates/hs-federation/src/transport/join.rs`:

- **`make_join` is fully real.** It reads the room's actual current state and actual forward
  extremities (see the two new `RoomDataSource` methods below) and builds a genuine unsigned
  `m.room.member` template: real `room_version` (checked against the requester's `?ver=` list,
  `M_INCOMPATIBLE_ROOM_VERSION` if none match), real `prev_events`/`depth` from the room's actual
  extremity, and real `auth_events` selected via `hs_state::auth::expected_auth_types` against
  current state -- the actual algorithm the spec names, not a re-derivation of it. It also runs
  `hs_state::auth::check_event_auth` as a courtesy pre-check (rejects a hopeless join, e.g. no
  invite in an invite-only room, before handing out a template that `send_join` would refuse
  anyway).
- **`send_join` validates for real**: `crate::inbound::verify_pdu` (signature + content hash) on
  the submitted event, shape checks (it is `m.room.member`, `content.membership == "join"`, its
  `room_id` matches the path, and -- the check that actually matters for security -- its
  `sender`'s server matches the requester that signed the HTTP request, so one server cannot submit
  a join "on behalf of" another server's user), then `hs_state::auth::check_event_auth` against
  real current state. What it does **not** run: `hs_state::auth::check_auth_events_selection` (the
  state-*independent* half of the spec's checks) or the spec's required three-snapshot check
  (implied-by-`auth_events`, before-the-event, current-at-receipt) -- `hs-room`'s own
  `pipeline.rs` documents this exact gap as "still track 06's job" for a *locally* authored event,
  and it is out of scope here too, for the same reason: doing it properly needs the DAG-walking
  machinery this session did not have budget to build on top of the persistence gap below.
- Two new `RoomDataSource` methods (`crates/hs-federation/src/room_source.rs`), needed by both:
  `room_version(room_id) -> Option<String>` and `forward_extremities(room_id) ->
  Result<Vec<(String, i64)>, RoomSourceError>`, plus `state_for_join(room_id) ->
  Result<StateForJoin, RoomSourceError>` (current state + its auth chain, **not** gated by
  `is_visible_to` -- handing a prospective joiner's server what it needs to construct and authorize
  a join is the entire point of the handshake, not a bypass of the membership check that gates
  ordinary reads; the join itself is authorized separately). `hs-cli`'s `RegistryRoomSource`
  implements all three against `hs-room`'s existing public `RoomActor` API
  (`full_state`/`paginate`/`room_version`) -- no `hs-room` change needed for the read side.
- **Only `EventsReferenceFormat::V2IdOnly` room versions are supported** (bare event-ID references
  in `prev_events`/`auth_events` -- room versions 3 through 12). Room versions 1-2
  (`V1WithHash`, `[event_id, {"sha256": ...}]` pairs) are explicitly rejected by `make_join`/
  `send_join` with `UnsupportedRoomVersion`: computing that pair for a forward extremity needs the
  extremity's full event body, which `forward_extremities` does not carry (only `(id, depth)`).
  This server's own default and only-tested room version is 11 (`V2IdOnly`), so this is a narrow,
  named gap, not a silent one.
- **Faster joins (MSC3706/MSC4229, `omit_members`) are explicitly out of scope**, as this session's
  brief said to declare up front. Every response is the full, unabridged state and auth chain;
  `members_omitted` is always `false`.
- v2's response adds `event` (the verified event, echoed back) and `members_omitted: false` on top
  of v1's `state`/`auth_chain`/`origin`.

### The one gap this session could not close: persisting a newly-received event

Both `/send` and `send_join` bottom out at `RoomWriteSink::accept_verified_event`. Every
implementation this session can offer -- `crate::inbound::StaticWriteSink` (this crate's own
tests) and `hs_cli::federation::RegistryWriteSink` (the real one, wrapping `hs-room`'s
`RoomRegistry`) -- can only ever report success for an event **this server already holds** (checked
by event ID against the resident `RoomActor`). For a genuinely new event, both return a distinct,
documented error (`error` contains "cannot yet persist"; `send_join`'s HTTP response carries
`errcode: M_HS_INBOUND_INGESTION_UNSUPPORTED`, `501`) rather than a generic seam response or -- far
worse -- a fake success.

**Why**: `hs-room`'s `RoomActor` (`crates/hs-room/src/actor.rs`) has exactly two ways to add an
event to a room, and both build and sign a **new** event from scratch using this server's own
identity (`send_event`/`send_event_citing`, both calling `pipeline::build_and_authorize`, which
takes a `NewEvent { event_type, state_key, sender, content, redacts }` -- content and metadata
only, never an already-built `Event`). There is no entry point that takes an already-signed,
already-hashed, foreign `hs_model::Event` and persists it as-is. `hs-room`'s own `pipeline.rs` names
this precisely: its doc comment says the general inbound-event check "is still track 06's job --
see `docs/design/04-room-actor-protocol.md`'s `Command::PersistInbound`" -- a command that was
*named* in the design doc but never implemented, by either track. I do not own `crates/hs-room` and
did not edit it. I considered and rejected re-authoring the received event locally (calling
`membership_action`/`send_event` with the remote user as `sender` but signed by this server's own
key): that would silently produce a cryptographically wrong event (signed by the wrong server for
its sender's domain) that every other real homeserver in the room would reject, and would give this
event a *different* ID than the one the joining server has -- exactly the kind of half-built
handshake this project's own conventions call more dangerous than an honest error.

**What `hs-room` needs, concretely, to lift this** (for whoever picks up
`docs/design/04-room-actor-protocol.md`'s `Command::PersistInbound`, likely track 04):
a `RoomActor` entry point along the lines of

```rust
/// Persists an already-verified, already-authorized foreign event (its signature and content hash
/// checked by the caller, e.g. `hs_federation::inbound::verify_pdu`) exactly as received: no
/// re-signing, no `event_id` regeneration. Computes `depth`/forward-extremity bookkeeping from the
/// event's own `prev_events` (which may not be this actor's current extremities -- unlike
/// `send_event`, this does not get to assume convergence) and feeds `hs_state::api::StateStore` the
/// same way `RoomActor::persist` already does for a locally-built event.
pub fn accept_remote_event(&mut self, event: hs_model::Event) -> Result<(), RoomError>;
```

with the caller (this crate, or whoever owns the inbound pipeline) responsible for everything
`hs_state::auth` needs *before* calling it (the three-snapshot check this session also did not
build) and this method responsible only for the mechanical parts `RoomActor::persist` already knows
how to do (intern, write, update the state store, update forward extremities, publish a
`RoomUpdate`) minus the "build and sign a new event" half that does not apply to a foreign one.

### Mutation-tests performed this session

Per this session's instructions, both required guarantees were actually broken (not just reasoned
about) and the tests re-run to confirm they catch it, then reverted:

1. **Signature check disabled**: in `crate::inbound::verify_pdu`, short-circuited to `return
   Ok(event)` immediately after the key lookup, skipping the `signing::verify_object` call
   entirely. Result: `inbound::tests::verify_pdu_rejects_a_tampered_signature_with_hash_intact`
   failed, as did `hs-cli`'s end-to-end `federation_writes::send_rejects_a_pdu_with_a_tampered_signature`
   (a real signed PUT against the real composed router). **Notably, two *existing* tests did
   *not* catch this mutation**: `verify_pdu_rejects_a_tampered_body` (it tampers the *content*,
   which the independent content-hash check catches before signature verification is ever
   reached) and `verify_pdu_rejects_a_signature_from_the_wrong_key` (the wrong-key scenario is
   caught by the key-lookup step, upstream of `verify_object`). This is exactly the kind of gap
   this project's conventions warn about -- a test with the right *name* that is not actually
   exercising the code path it claims to -- so a new test,
   `verify_pdu_rejects_a_tampered_signature_with_hash_intact`, was added specifically to tamper
   *only* the signature bytes with the hash left intact, isolating the check. That test and the
   `hs-cli` integration test are the ones that actually prove this guarantee; the two older ones
   prove different (also real) guarantees and are kept.
2. **Idempotency disabled**: in `crate::inbound::process_transaction`, wrapped the `(origin,
   txn_id)` cache lookup in `if false { ... }`. Result:
   `inbound::tests::replaying_a_transaction_id_does_not_reprocess` failed immediately (its sink
   panics if called a second time for the same transaction, which is exactly what the disabled
   idempotency check let happen).

Both mutations were reverted immediately after confirming the failure; `cargo test -p hs-federation
--lib` is green at 114/114 with both reverted (see "Verification").

## Wiring the integration lead must add

Not done this session (`crates/hs-cli/src/serve.rs` is owned by the integration lead this
session). In `serve.rs`, the existing federation-mounting block

```rust
if let Some((state, x_matrix, own_keys, server_name)) = federation {
    let (federation_router, federation_manifest) =
        hs_federation::transport::router(state, x_matrix);
    // ...
    builder = builder
        .get("/_matrix/key/v2/server", /* ... unchanged ... */)
        .merge_router("/_matrix/federation/v1", federation_router, federation_manifest.routes);
}
```

needs to become (clone `state`/`x_matrix` -- `FederationState` derives `Clone`, `x_matrix` is
already an `Arc` -- so both routers see the same data sources and the same key cache):

```rust
if let Some((state, x_matrix, own_keys, server_name)) = federation {
    let (federation_router, federation_manifest) =
        hs_federation::transport::router(state.clone(), x_matrix.clone());
    let (federation_router_v2, federation_manifest_v2) =
        hs_federation::transport::router_v2(state, x_matrix);
    // ...
    builder = builder
        .get("/_matrix/key/v2/server", /* ... unchanged ... */)
        .merge_router("/_matrix/federation/v1", federation_router, federation_manifest.routes)
        .merge_router("/_matrix/federation/v2", federation_router_v2, federation_manifest_v2.routes);
}
```

That is the only change needed there. `crate::federation::build_mount` and
`crate::federation::manifest_only_mount` (both in `crates/hs-cli/src/federation.rs`, which I do
own) already build a `FederationState` with the new `write_sink`/`transactions` fields populated
for real, so no other `hs-cli` change is needed to pick this up -- `cargo run -p hs-cli --bin hs
routes-manifest` will then list `send_join`/`send_leave`/`invite` under
`/_matrix/federation/v2/...` for the first time.

## Verification

```
cargo fmt -p hs-federation -p hs-cli                                   # applied, no diffs after
cargo clippy -p hs-federation --all-targets -- -D warnings             # clean
cargo clippy -p hs-cli --all-targets --no-deps -- -D warnings          # clean (see note below)
cargo test -p hs-federation --lib                                     # 114 passed, 0 failed
cargo test -p hs-cli --test federation_reads                          # 7 passed, 0 failed
cargo test -p hs-cli --test federation_writes                         # 6 passed, 0 failed (new this session)
cd crates/hs-federation/fuzz && cargo check                            # clean (5 bins type-check, unchanged)
```

Note on the `hs-cli` clippy command: without `--no-deps`, `cargo clippy -p hs-cli` also lints
every path-dependency crate in the workspace as part of the same build, and `hs-push` (not this
track's crate) currently fails `-D warnings` on unrelated `result_large_err` lints. That is
pre-existing and not something this session touched; `--no-deps` scopes the check to `hs-cli`'s
own code, which is clean.

`cargo run -p hs-cli --bin hs routes-manifest` still only lists the v1 federation spellings
(`send_join`/`send_leave`/`invite` under `/_matrix/federation/v1/...`) because `router_v2` is not
mounted in `serve.rs` yet — see "Wiring the integration lead must add" above.

## In progress

Nothing mid-file. Everything listed under "Done" (second/third session) and above (fourth session)
is a complete, tested unit, except the one named gap (`RoomWriteSink` cannot persist a new event —
see above) which is honestly reported as a gap, not left half-built.

## Next (for whoever resumes this track)

Superseded from earlier sessions' lists (wiring `hs-federation` into `hs serve`, the real
`RoomDataSource` adapter, `HttpKeyServerFetcher`, the key-server axum handlers) are all done as of
the third and fourth sessions and removed from this list. What remains:

1. ~~**`Command::PersistInbound` on `hs-room`'s `RoomActor`**~~ -- built by track 04 between
   sessions (`RoomActor::accept_remote_event`), consumed by the fourth session
   (`RegistryWriteSink`) and, as of this (fifth) session, actually reachable end to end: the
   backfill loop means an event citing history from before a join can now be resolved, not just
   accepted when its ancestors happen to already be present.
2. **The three-snapshot auth check** for both `/send`'s PDUs and `send_join`'s event
   (implied-by-`auth_events`, before-the-event, current-at-receipt state, per the server-server
   spec) -- this session's `send_join` only checks against *current* state, and `/send`'s PDUs are
   not authorization-checked at all (only signature/hash-verified) since there is nothing to gain
   from authorizing an event this server cannot yet persist either way. Both depend on (1) existing
   first: authorization checking three snapshots of a DAG this server cannot record is a check with
   nowhere to attach its result.
3. **`make_leave`/`send_leave`, `make_knock`/`send_knock`, `invite` v1/v2**: still seams (correctly
   mounted, including at the now-fixed v2 path for `send_leave`/`invite`). Same shape of work as
   `make_join`/`send_join`, and blocked on the same persistence gap for the "send" half of each
   pair; `make_leave`/`make_knock` (the read/template half) could be done independently and would
   follow `make_join`'s pattern closely.
4. **Server ACL enforcement wiring**: `acl.rs`'s `is_allowed` function exists and is tested in
   isolation, but nothing calls it yet -- it needs threading into (a) the inbound accept path
   (natural home: `crate::inbound::process_transaction`, reading the room's `m.room.server_acl` via
   a new `RoomDataSource` method) and (b) `crate::client::FederationClient::send`. Unblocked by this
   session's work (the `RoomDataSource` real adapter exists now) but not done this session --
   budget went to the write paths per this session's explicit priority order.
5. **A `KvBackend`-backed `TransactionStore`**, mirroring `destination_store.rs`'s
   `KvDestinationStore` pattern, if a transaction retry surviving a server restart ever turns out to
   matter in practice (see `crate::inbound`'s module doc for why in-memory was judged sufficient
   this session).
6. **Discovery result caching in `client.rs`** (carried over, unchanged): `client_for` caches the
   pinned `reqwest::Client` per destination but only invalidates it when a *fresh* `resolve()` call
   produces a different `ResolvedServer` -- no proactive TTL-based re-resolution independent of a
   new `send` call. Acceptable for now, noted as a gap.
7. **Backfill only ever asks `origin`** (new this session): `resolve_missing_ancestors` fetches from
   the one server that sent the transaction, never tries a different member of the room if `origin`
   is unreachable or uncooperative. Real deployments often have several servers in a room that could
   answer; this session deliberately scoped to the single-peer case (it is what closes the
   join-then-stall gap named in `docs/next-steps.md`) and left multi-peer fallback as a named gap
   rather than a half-built heuristic for picking among peers this crate has no signal to rank.
8. **`crate::acl::is_allowed` is still not threaded into the backfill path either** -- it inherits
   item 4's gap (nothing calls `is_allowed` anywhere yet), not a new one: once item 4 is done,
   `resolve_missing_ancestors`'s calls to `AncestorFetcher::fetch_backfill` go through the same
   `FederationClient::send` every other outbound call does, so wiring ACL into `FederationClient`
   once covers this path too, with no separate change needed here.

## Blockers

None. The one blocker recorded in earlier sessions (`RoomActor::accept_remote_event` needing to
exist on `hs-room`) was resolved by track 04 between the fourth and fifth sessions; this session's
work built entirely on top of it and needed nothing further from any other track.

## Interfaces provided

- **`crate::client::FederationClient`** (tracks 08, 09, 11 per the brief): `new(...)` takes an
  owned server name, a `SigningKeyPair`, a `ClientConfig`, and `Arc<dyn DestinationStore>` /
  `Arc<dyn WellKnownFetcher>` / `Arc<dyn SrvResolver>` / `Arc<dyn AddrResolver>`; `.send(destination,
  method, path, body) -> Result<FederationResponse, ClientError>` is the one call site. Wired into
  `hs-cli` since the third session.
- **`crate::room_source::RoomDataSource`**: the read-only room seam, now with three more methods
  (`room_version`, `forward_extremities`, `state_for_join`) added this session for the join
  handshake. `hs_cli::federation::RegistryRoomSource` is the real implementation over `hs-room`;
  `InMemoryRoomSource`/`FakeRoom` remain available for any track's own tests.
- **`crate::inbound::{verify_pdu, process_transaction, RoomWriteSink, WriteOutcome, WriteRejected,
  TransactionStore, InMemoryTransactionStore, StaticWriteSink}`**: the inbound PDU-verification and
  transaction-envelope primitives. `WriteRejected` gained a structured `missing_ancestors: Vec<String>`
  field this (fifth) session, plus `WriteRejected::other`/`WriteRejected::missing_ancestors`
  constructors -- any other implementation of `RoomWriteSink` should use these rather than
  constructing the struct literal directly, so a future field addition here does not need every
  implementation to change. `process_transaction`'s signature grew two parameters this session
  (`ancestor_fetcher: Option<&dyn crate::backfill::AncestorFetcher>`, `backfill_limits:
  &crate::backfill::BackfillLimits`) -- pass `None` and `&BackfillLimits::default()` to keep the old
  behaviour (report the gap, do not try to close it).
- **`crate::backfill::{AncestorFetcher, AncestorFetchError, BackfillLimits, BackfillGiveUpReason,
  resolve_missing_ancestors}`** (new this session): the backfill resolution loop described above.
  `FederationClient` implements `AncestorFetcher`; anything that wants to trigger a backfill
  resolution outside `process_transaction` (a future explicit "resync this room" admin action,
  say) can call `resolve_missing_ancestors` directly against any `RoomWriteSink`.
- **`crate::client::FederationClient::backfill(destination, room_id, from_event_ids, limit) ->
  Result<Vec<Value>, ClientError>`** (new this session): the outbound `/backfill` client, returning
  raw unverified PDUs -- callers must run each through `crate::inbound::verify_pdu` themselves.
- **`crate::join::{make_join, send_join, JoinTemplate, SendJoinResult, JoinError, RoomWriteSink}`**
  (new this session): the join-handshake logic, usable directly by anything that wants to build or
  validate a join without going through the axum layer (e.g. a future differential test against
  recorded Synapse traffic, per this track's definition of done).
- **`crate::transport::{FederationState, FederationQuerySource, InMemoryQuerySource, router,
  router_v2}`**: the federation router-fragment functions, both mounted in `hs-cli` as of the fourth
  session. `FederationState` gained two more fields this (fifth) session:
  `ancestor_fetcher: Option<Arc<dyn crate::backfill::AncestorFetcher>>` (`None` disables backfill)
  and `backfill_limits: crate::backfill::BackfillLimits`; any other track constructing one directly
  (none do today, per a repo-wide grep) needs to supply both -- `None` and `BackfillLimits::default()`
  reproduce the pre-this-session behaviour exactly.
- **`crate::xmatrix::{sign_request, verify_x_matrix, XMatrixContext}`**: request signing for any
  track that needs to make an authenticated federation call directly (though `FederationClient`
  should normally be preferred), and the verification middleware/context type for whoever wires
  the federation listener into `hs-cli`.
- **`crate::keys::{OwnSigningKeys, RemoteKeyCache, KeyServerFetcher, DynRemoteKeyCache}`**: key
  management for any track that needs to verify a federation signature outside the request path
  (e.g. verifying a signed `m.room.third_party_invite`).
- **`crate::acl::{ServerAcl, is_allowed}`**: the one ACL evaluation function, for whoever wires
  inbound/outbound enforcement (see "Next" item 4).

## Interfaces needed

- ~~**Track 04**: the real `RoomDataSource` adapter over `RoomActorHandle`.~~ Built in the third
  session as `hs_cli::federation::RegistryRoomSource`. ~~What it still needs from track 04 is a
  **state-at-an-event** query~~ -- also lifted the third session (`RoomActor::state_at_event`);
  `/state` and `/state_ids` now answer for any event this server holds, not just the newest.
- ~~**Track 04, the real blocker**: a `RoomActor` entry point that accepts an already-verified,
  already-signed foreign event and persists it as-is.~~ Built by track 04 between the fourth and
  fifth sessions (`RoomActor::accept_remote_event`, `docs/design/04-room-actor-protocol.md`'s
  `Command::PersistInbound`). Nothing further needed from track 04 as of this session.
- **Nothing new needed from another track this (fifth) session** -- the backfill loop is entirely
  built from interfaces this crate already owned (`RoomWriteSink`, `RoomDataSource`,
  `FederationClient`) plus one already-existing `hs-room` entry point (`accept_remote_event`).
- **Track 08 (E2EE)**: `/user/keys/claim` and `/user/keys/query` remain mounted as seams pending
  track 08's contract, per the brief's joint-ownership note.
- ~~**hs-cli / whoever owns `hs serve`'s wiring**: needs to call `crate::transport::router`,
  `crate::client::FederationClient::new`, and load `OwnSigningKeys` at startup.~~ Done in the
  third session. **New this session**: `hs serve`'s wiring also needs to mount `router_v2` at
  `/_matrix/federation/v2` -- see "Wiring the integration lead must add" above; this one is not
  done yet.

## Decisions made

New this (fifth) session:

- **The backfill target is always `origin`** (the server that sent the transaction reporting the
  gap), never a different member of the room. Considered trying every joined server this crate
  knows about and rejected it for this session: this crate has no signal to rank peers by
  reliability, and falling back through an unranked list on every failure risks turning one
  hostile or slow peer into several round-trips' worth of wasted work for a gap that peer alone
  cannot close either. `origin` is also the server most likely to actually have the history (it is
  the one that just cited it), so it is the correct first (and, this session, only) choice. Recorded
  as "Next" item 7, not treated as a design flaw.
- **`/backfill` was chosen over `/get_missing_events` as the fetch primitive**, per the brief's
  explicit permission to pick "as the spec and the situation decide". `/backfill` takes exactly
  "the IDs I'm missing" and a limit and walks backwards from them, which is precisely this
  session's situation (an event named specific ancestor IDs this server does not hold);
  `/get_missing_events` additionally requires communicating this server's own `earliest_events`
  frontier to the peer, which needs a concept of "this room's backward frontier from this server's
  point of view" that `RoomDataSource` does not expose today and that this session judged
  unnecessary complexity for the case actually being solved. `/get_missing_events`'s outbound
  client was not built this session; noted as a possible future addition, not a gap in what this
  session was asked to close.
- **`WriteRejected` grew a structured field instead of a new error type.** Considered a
  `Result<WriteOutcome, WriteError>` where `WriteError` is an enum with a `MissingAncestors(Vec
  <String>)` variant, and rejected it: every existing caller (`crate::join::send_join`,
  `crate::inbound::process_transaction`, `hs_cli::federation::RegistryWriteSink`, this crate's own
  tests) already matches on `WriteRejected { error, .. }` as a struct; changing the error's *type*
  would touch every one of those call sites for a change that is really just "add one more field
  most callers will ignore". The two new constructors (`WriteRejected::other`,
  `WriteRejected::missing_ancestors`) keep every construction site from having to remember to set
  the new field explicitly, and are the only sites this session changed.
- **The resolution loop tracks a `pending` worklist across rounds, not just within one**, per the
  real multi-hop bug this session's own test caught (see "Fifth session" above). This is the one
  piece of this session's design that changed shape *after* being written and tested, not before --
  recorded here so the reasoning is not lost: an event fetched in round N that cannot yet be
  persisted must still be attempted again in round N+1 once whatever blocked it lands, not
  discarded the moment its first attempt fails.
- **A hard-rejected fetched event (bad auth, malformed, ...) is silently dropped, not retried and
  not reported as part of the eventual give-up reason.** Considered surfacing it (e.g. a
  `BackfillGiveUpReason::AncestorRejected(id, reason)` variant) and decided the extra type
  complexity was not worth it: a hard rejection during backfill is not actually a *backfill*
  failure -- it means a fetched event failed the same authorization check any inbound event would
  fail, which is already a defended, tested code path (`crate::inbound::verify_pdu`,
  `RoomActor::accept_remote_event`'s own authorization). The caller's eventual retry of the
  original event will report whatever ancestor is *still* actually missing (if the hard-rejected
  event was itself required), which is the information that actually matters to whoever reads the
  `/send` response.

New this (fourth) session:

- **`hs-state` was added as a direct dependency of `hs-federation`** (`Cargo.toml`, path
  dependency, not a `[workspace.dependencies]` entry -- matches how `hs-model`/`hs-kv`/etc. are
  already declared here). `crate::join` calls `hs_state::auth::{check_event_auth,
  expected_auth_types}` directly rather than re-deriving the join-rules/power-level/membership auth
  rules a second time, per this session's explicit instruction ("not a second copy of the auth
  rules"). No cycle risk: `hs-state` depends on nothing above it in the crate graph.
- **`RoomWriteSink` is a new, separate seam from `RoomDataSource`**, not a fourth read method
  bolted onto the existing (deliberately read-only, per its own doc comment) trait. `/send` and
  `send_join` share exactly one write seam rather than each inventing their own, which is what
  makes the "one honest gap" in this session's write-up a single, named thing instead of two.
- **`send_join`'s persistence-gap error is a `501` with a distinct errcode
  (`M_HS_INBOUND_INGESTION_UNSUPPORTED`)**, not a `200` with a state/auth_chain response that
  claims success. Considered and rejected: returning `200` would tell a real remote server its join
  succeeded when this server recorded nothing, which is a worse failure mode than an honest error --
  the remote would proceed to sync and participate in a room it believes it joined while this
  server's data never reflects the membership.
- **Re-authoring a received foreign event with this server's own identity, to make persistence
  "work", was considered and rejected** (see "The one gap this session could not close" above for
  the full reasoning): it would produce a cryptographically wrong event (signed by the wrong
  server for its sender's domain) with a different ID than what the joining server holds, which
  every other real homeserver in the room would reject on sight. An honest, typed error that
  proves everything *up to* persistence is real is safer than a handshake that looks complete and
  is quietly wrong.
- **`/send`'s PDUs are not authorization-checked against room state this session, only
  signature/hash-verified.** Considered doing a single-snapshot `check_event_auth` (as `send_join`
  does) and rejected it for `/send` specifically: since no PDU can be persisted regardless of the
  outcome (see the gap above), an auth check here would only ever change *which* error message an
  operator sees, at the cost of building `FlatState` for every PDU in every transaction. Revisit
  once (1) under "Next" exists and an accepted PDU has somewhere to go.
- **Only `EventsReferenceFormat::V2IdOnly` room versions are supported by the join handshake**
  (documented above under item 3) -- a deliberate, narrow scope decision given this server's
  default and only-tested room version (11) is already in that family, rather than building
  `V1WithHash` reference-pair support for forward extremities that would need a `RoomDataSource`
  signature change (fetching full event bodies, not just `(id, depth)`) to support two room
  versions this server has never been run against.

From the third session (the second session's decisions, still valid, are below under "Decisions
made (first session, unchanged)"):

- **`ClientConfig` is decoupled from `hs_config::FederationConfig`** (its own struct in
  `client.rs`, with `DomainPolicy`/`IpPolicy` built from plain `Vec<String>` CIDR/domain lists via
  `from_cidrs`/`new`, not from `hs-config`'s type directly). Keeps `client.rs` testable without a
  dependency on `hs-config`'s validation/schema machinery and keeps the conversion (a handful of
  lines) at the wiring site where `hs-config` is already in scope, rather than making this crate's
  core client logic depend on another crate's config schema shape. `hs-config` remains a
  `Cargo.toml` dependency of this crate (used nowhere yet — it was speculatively added in the
  first session's plan; still fine to keep, since `ServerConfig`/`FederationConfig` will be read at
  the `hs-cli` wiring point that lives logically alongside this crate's own types, even if not
  literally inside `client.rs`).
- **`ClientConfig::scheme` is a test-only seam, `"https"` by production default, never settable by
  any config loader.** Added specifically so `client.rs`'s own tests could exercise real discovery
  + signing + pooling + concurrency + backoff against `hs_testkit::FakeFederationPeer` over a real
  loopback socket, without also having to stand up a self-signed TLS certificate and trust chain
  (which would mostly be testing `reqwest`'s already-tested TLS stack, not this crate's logic).
  Documented inline on the field itself; not exposed through `hs-config`.
- **The `X-Matrix` middleware re-parses the `Authorization` header per read-route handler**
  (`transport::read_routes::requesting_server`) rather than threading the already-verified
  `origin` through axum request extensions from `verify_x_matrix`. This is a deliberate
  simplification for this pass, not an oversight: parsing is infallible at that point (the layer
  already proved the header is well-formed enough to have signed correctly) and cheap, and it
  avoids introducing an extension-passing contract between two independently-testable modules
  (`xmatrix` and `transport`) before there's a second consumer that would justify it. Revisit if a
  profiler ever cares, or if a second handler module needs the same value and the duplication
  starts to feel real rather than theoretical.
- **`RoomDataSource::get_event_by_id`** (room-less event lookup) was added beyond what the first
  session's plan enumerated for the trait, because `/event/{eventId}`'s actual spec path shape has
  no room ID in it — a server has to know which room an event belongs to before it can apply the
  membership check. `InMemoryRoomSource`'s implementation linearly scans every room, which is fine
  for a test fake and would not be for a real adapter (track 04's adapter should back this with an
  actual event-ID index, not a scan).
- **`edu.rs` was added, beyond the original plan's file list**, purely to give the required "EDU
  JSON" fuzz target a real parser to call rather than fuzzing raw `serde_json::Value` parsing with
  no structural validation at all. It intentionally does not interpret any `edu_type`'s `content`
  schema — that's real work for whichever session first implements EDU handling inside `/send`.
- **`RemoteKeyCache::ingest_response` and `discovery::parse_well_known_body` were made `pub`**
  (the former was previously private, the latter was extracted from being inline in
  `HttpWellKnownFetcher::fetch`) specifically so both are independently fuzzable without needing a
  live network fetcher to drive them. This also had the side benefit of eliminating a
  near-duplicate parsing path in `HttpWellKnownFetcher::fetch`, which now calls the same function
  the fuzz target does.
- **`transport::router` composes both sub-routers into one `Builder` before calling `.build()`
  once**, rather than building two separate `axum::Router`s and merging them via
  `Builder::merge_router`. Not a style preference: `Builder::merge_router` nests the incoming
  router under a prefix via `axum::Router::nest`, and axum panics ("Nesting at the root is no
  longer supported") when that prefix is `""` — which it must be here, since `read_routes` and
  `seams` both register spec-relative paths (`/version`, `/send/{txnId}`, ...) that are meant to
  live at the same level, not under a sub-prefix. Discovered by the test suite immediately (both
  `transport::tests` cases panicked at construction), not by production use. `read_routes` and
  `seams` therefore expose `pub(super) fn add_routes(builder) -> builder` (composable into a
  shared `Builder`) rather than `pub(super) fn router() -> (Router, Vec<Route>)`.

## Decisions made (first session, unchanged)

- **`RoomDataSource` is a trait owned by `hs-federation`, not a dependency on `hs-room`** — see
  threat model section 5. Confirmed correct by this session's own experience: `InMemoryRoomSource`
  made every `transport::read_routes` handler fully testable today, with the real adapter still
  entirely someone else's future work.
- **No federation route is exposed before `X-Matrix` verification wraps the whole router** — now
  enforced by an actual test (`transport::tests::every_route_is_behind_the_x_matrix_layer`), not
  just a stated intention.
- **Join/leave/knock/invite handshakes and `/send` ship as seams this pass** — done exactly as
  specified: signature verification (via the shared layer), a shared typed 501, nothing else.
- **`hickory-resolver` is the DNS resolver** — added and in use (`discovery::HickoryResolver`).
- **HTTP/1.1-only to federation peers** — committed in code (`client.rs`'s `client_for`:
  `.http1_only()` on every outbound `reqwest::Client`).

## Reuse considered (decision 0007)

Unchanged from the first session's analysis (`hickory-resolver`, `reqwest`, `hs-model`'s
canonical/signing/hashing, `ruma-federation-api` considered-but-not-adopted-for-handlers), plus:

- **`ipnet`** (new this session): adopted for CIDR parsing/containment
  (`client::IpPolicy`) rather than hand-rolling prefix-length arithmetic over `IpAddr`. Already
  present transitively via `hickory-net`; promoting it to a direct dependency costs nothing new in
  the dependency tree and avoids a bug-prone reimplementation of CIDR containment (off-by-one
  errors in prefix-length masking are a classic source of exactly the kind of SSRF-adjacent bug
  this check exists to prevent).
- **`rcgen`** (dev-dependency only, unused in the end): added anticipating a real-TLS test harness
  for `client.rs`'s integration tests, then not used — `ClientConfig::scheme`'s plaintext-test seam
  turned out to test this crate's own logic more directly without exercising `reqwest`'s TLS stack
  (someone else's already-tested code). Left in `Cargo.toml` as a dev-dependency in case a future
  session wants a real-TLS test after all; flagged here so it isn't mistaken for dead weight
  without an explanation.

## Shared dependencies added

None this (fifth) session -- the backfill loop is built entirely from crates already depended on
(`tokio` for `time::timeout`, `async-trait`, `ruma`, `serde_json`), and `hs-cli`'s two new
integration tests use only `axum`/`tokio` (already plain, non-dev dependencies of `hs-cli`) rather
than adding `hs-testkit` as a dev-dependency there (see "Decisions made" -- not actually a decision
this session made explicitly, but worth noting: `hs-testkit::fake_federation::FakeFederationPeer`
would have been a natural fit for the two new `hs-cli` tests' "remote server" double, and *is* used
for `crate::client`'s own new unit test in this crate, but `hs-cli/Cargo.toml` is not a file this
session owns, so the `hs-cli` tests build their own minimal axum catch-all instead).

- **`hickory-resolver`** (0.26, workspace): added the second session. Features: `tokio` (brings
  `system-config` along via its own defaults).
- **`ipnet`** (2, workspace): added the second session, for `client::IpPolicy`'s CIDR containment
  checks.
- **`hs-state`** (path dependency, this crate's own `Cargo.toml`, not a `[workspace.dependencies]`
  entry): added this (fourth) session, so `crate::join` can call the real
  `hs_state::auth::{check_event_auth, expected_auth_types}` rather than re-deriving the auth rules.
  See "Decisions made" above.

The workspace-level entries (`hickory-resolver`, `ipnet`) are noted with "Added by track 06"
attribution comments in the root `Cargo.toml`; `hs-state` needed no root `Cargo.toml` change since
internal crate-to-crate path dependencies are declared directly in each crate's own manifest (the
same way this crate already depends on `hs-model`/`hs-kv`/etc.).
