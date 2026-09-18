# 06 Federation: status

Updated: 2026-09-18 (second session). The first session wrote the threat model and the plan below
but stopped before any crate code existed. This session implemented items 1-7 of that plan against
real, adversarial-input-oriented tests. `crates/hs-federation` is no longer a placeholder: 93
passing tests, `cargo clippy -p hs-federation --all-targets -- -D warnings` clean, five fuzz
targets that type-check. Read this file before touching `hs-federation` further.

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

## Verification

```
cargo check -p hs-federation                                          # clean
cargo test -p hs-federation --lib                                     # 93 passed, 0 failed
cargo clippy -p hs-federation --all-targets -- -D warnings             # clean
cargo fmt -p hs-federation                                             # applied, no diffs after
cd crates/hs-federation/fuzz && cargo check                            # clean (5 bins type-check)
```

`cargo check --workspace` currently fails, but **not from anything in this crate**: `hs-cli`
fails to build (`crates/hs-cli/src/serve.rs`, calls to
`hs_http::router::Builder::merge_router` with a `RouteManifest` where `Vec<Route>` is now
expected) because another track changed `hs_http::router::Builder::merge_router`'s signature
concurrently with this session. This is pre-existing/concurrent breakage in a crate this track
does not own and was not touched by this session — confirmed by `cargo check -p hs-federation`
passing standalone. Flagging here so it isn't mistaken for something this session broke; not
fixed, per the "do not edit other tracks' crates" rule.

## In progress

Nothing mid-file. Everything listed under "Done" is a complete, tested unit.

## Next (for whoever resumes this track)

1. **Wire `hs-federation` into `hs-cli serve`**, the way `hs-media`/`hs-room`'s routers will be
   (none of the three are mounted in `hs-cli` yet per the first session's research — this is not
   regression, it was already true). Needs: loading `OwnSigningKeys` from
   `ServerConfig::signing_key_path`, building a `DynRemoteKeyCache` with a real
   `HttpKeyServerFetcher: KeyServerFetcher` (not written yet — only the trait and the in-memory
   test fetchers exist; the real implementation is a thin wrapper: resolve via `discovery::resolve`,
   GET `/_matrix/key/v2/server`, cap the body like the well-known fetcher does), and a small
   `hs_config::FederationConfig -> client::ClientConfig` conversion function (deliberately not
   written in this crate — `ClientConfig` is decoupled from `hs-config` on purpose, see "Decisions
   made"; the conversion belongs at the wiring site).
2. **The real `RoomDataSource` adapter onto `hs-room`**, once track 04's state-engine wiring gap
   (`docs/rfcs/0010`) closes — this was explicitly out of scope for this session per the original
   sequencing instructions, and remains so. `InMemoryRoomSource` is a test fake only.
3. **`/send` and the join/leave/knock/invite handshakes**, against real room state once (2) exists.
   Still the highest-risk surface in the whole track (`PLAN.md`'s own callout). The threat model
   (section 2.5) and this session's seam handlers are the starting point; do not build partial
   logic into the seam handlers before the state engine is ready — an honest 501 is safer than a
   half-built handshake, and that's what's there now.
4. **Server ACL enforcement wiring**: `acl.rs`'s `is_allowed` function exists and is tested in
   isolation, but nothing calls it yet — it needs to be threaded into (a) the inbound accept path
   once `/send` is real (item 3 above) and (b) `crate::client::FederationClient::send`, both
   reading the room's current `m.room.server_acl` state via `RoomDataSource`, which itself needs
   item 2 above. Recorded as its own item because the brief calls out ACL enforcement "in both
   directions" as its own numbered priority (6), separate from the handshakes.
5. **`HttpKeyServerFetcher`**: the production `KeyServerFetcher` implementation (resolve via
   `discovery::resolve`, fetch `/_matrix/key/v2/server`, cap the body). Trivial once written — the
   `HttpWellKnownFetcher` in `discovery.rs` is the template to copy.
6. **The key-server and notary axum handlers themselves** (`/_matrix/key/v2/server`,
   `/_matrix/key/v2/query/{serverName}` and its batch POST form): `keys.rs` has every building
   block (`build_server_key_response`, `wrap_for_notary`) but no axum handler wraps them yet, and
   they are not mounted in `transport::router` — that router currently only covers
   `/_matrix/federation/v1/*`; the key endpoints live at `/_matrix/key/v2/*`, a separate mount
   point the caller (whoever wires `hs-cli`) needs to compose alongside it.
7. **Discovery result caching in `client.rs`**: `client_for` caches the pinned `reqwest::Client`
   per destination but only invalidates it when a *fresh* `resolve()` call produces a different
   `ResolvedServer` — there's no proactive TTL-based re-resolution independent of a new `send`
   call, and `discovery::resolve` itself is called fresh on every `send` (the `CachingWellKnownFetcher`
   decorator caches the well-known *fetch*, but SRV/A lookups are not cached at all). Acceptable
   for now (DNS resolvers typically cache internally) but noted as a gap, not a considered
   trade-off — revisit if profiling shows repeated SRV/A lookups are costly.

## Blockers

None for anything in "Done". Items 1-2 under "Next" are blocked on track 04's state-engine wiring,
per this track's own sequencing instructions (not a blocker on anything this track controls).

## Interfaces provided

- **`crate::client::FederationClient`** (tracks 08, 09, 11 per the brief): `new(...)` takes an
  owned server name, a `SigningKeyPair`, a `ClientConfig`, and `Arc<dyn DestinationStore>` /
  `Arc<dyn WellKnownFetcher>` / `Arc<dyn SrvResolver>` / `Arc<dyn AddrResolver>`; `.send(destination,
  method, path, body) -> Result<FederationResponse, ClientError>` is the one call site. Not yet
  wired into `hs-cli` (see "Next" item 1) but the API is stable and tested.
- **`crate::room_source::RoomDataSource`**: the trait whoever writes the `hs-room` adapter
  (this track or track 04) implements. `InMemoryRoomSource`/`FakeRoom` are available today for any
  other track's own tests that want a federation-shaped room double without depending on this
  crate's internals.
- **`crate::transport::{FederationState, FederationQuerySource, InMemoryQuerySource, router}`**:
  the federation router-fragment function (`hs-cli`'s eventual mount point, matching `hs-media`'s
  `router.rs` convention) plus the small query-data seam (`FederationQuerySource`) other tracks
  implementing real profile/device/alias storage will need to provide a real implementation of.
- **`crate::xmatrix::{sign_request, verify_x_matrix, XMatrixContext}`**: request signing for any
  track that needs to make an authenticated federation call directly (though `FederationClient`
  should normally be preferred), and the verification middleware/context type for whoever wires
  the federation listener into `hs-cli`.
- **`crate::keys::{OwnSigningKeys, RemoteKeyCache, KeyServerFetcher, DynRemoteKeyCache}`**: key
  management for any track that needs to verify a federation signature outside the request path
  (e.g. verifying a signed `m.room.third_party_invite`).
- **`crate::acl::{ServerAcl, is_allowed}`**: the one ACL evaluation function, for whoever wires
  inbound/outbound enforcement (see "Next" item 2).

## Interfaces needed

- **Track 04**: the real `RoomDataSource` adapter over `RoomActorHandle` (see "Next" item 1/2) —
  the concrete hooks (`query<T, F>`, `state_event`, `full_state`, `event_by_id`, `paginate`,
  `relations_of`) were already identified by the first session's research; unchanged this session.
- **Track 08 (E2EE)**: `/user/keys/claim` and `/user/keys/query` remain mounted as seams pending
  track 08's contract, per the brief's joint-ownership note.
- **hs-cli / whoever owns `hs serve`'s wiring**: needs to call `crate::transport::router`,
  `crate::client::FederationClient::new`, and load `OwnSigningKeys` at startup — none of this is
  wired yet (see "Next" item 1). This is genuinely this track's own remaining work, not a
  dependency on another track, just sequenced after everything in "Done".

## Decisions made

New this session (the first session's decisions, still valid, are below under "Decisions made
(first session, unchanged)"):

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

- **`hickory-resolver`** (0.26, workspace): added this session (the first session identified the
  need but did not add it). Features: `tokio` (brings `system-config` along via its own defaults).
- **`ipnet`** (2, workspace): added this session, for `client::IpPolicy`'s CIDR containment checks.

Both noted with "Added by track 06" attribution comments in the root `Cargo.toml`.
