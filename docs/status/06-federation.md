# 06 Federation: status

Updated: 2026-09-18. This is the first session on this track; it was cut short by an early
wrap-up instruction before any crate code was written. Read this whole file before touching
`hs-federation` — it is the only record of the design decisions and research this session did.

## Done

- **Threat model** (`docs/design/06-federation-threat-model.md`), the brief's explicit "before any
  code" deliverable. Real content, not a placeholder: per-mechanism threat/defence catalogue for
  discovery, the key server and notary, `X-Matrix` request auth (both directions), the transport
  server's read/query endpoints, the join/leave/knock/invite handshakes (threat-modelled now even
  though they ship as seams), the federation client, and server ACLs; a concrete resource-limits
  table (section 3) with named values for every limit this track needs to enforce; an explicit
  "out of scope this pass" section (4); and a section (5) recording the `RoomDataSource` seam
  decision below as a security-relevant boundary, not just an implementation convenience.
- Full research pass over every landed dependency this track consumes: `hs-model` (event parsing/
  validation via `Event::parse`, canonical JSON, hashing, redaction, and — critically —
  `signing.rs`, which already provides `sign_object`/`verify_object`/`verifying_key_from_base64`/
  `sign_bytes` over the same canonical-JSON machinery events use, so request signing and event
  signing share one implementation rather than two that could drift), `hs-state` (the frozen
  `StateStore` trait, `auth.rs`, `state_fetch::StateFetch`), `hs-kv`/`hs-tables` (transactional KV,
  typed keyspaces, interning — usable for a real `destinations` table), `hs-http` (`MatrixError`/
  `MatrixErrorCode`, the `router::Builder`/`RouteManifest`/`RouteMeta`/`Surface`/`AuthKind`
  machinery every listener uses, `error.rs`'s Matrix error shape), `hs-testkit::fake_federation`
  (a generic recording double — any-method/any-path axum router with a canned-response queue; it
  does **not** do X-Matrix signing/verification, so it is useful for exercising the outbound
  client's request shape and retry behaviour but not for testing inbound signature verification —
  that needs a real signed-request fixture built from `hs-model::signing` directly), `hs-room`
  (confirmed `Command::PersistInbound` is wired into the protocol but returns
  `RoomError::Internal` — see `docs/rfcs/0010-room-actor-state-store-seam.md` — and that
  `RoomActorHandle` exposes a generic `query<T, F>(&self, f: F) -> T` plus direct query methods
  (`state_event`, `full_state`, `event_by_id`, `members`, `paginate`, `relations_of`) that a future
  `RoomDataSource` adapter would call into), `hs-config::federation::FederationConfig` (already
  landed: `enabled`, `domain_allowlist`, `ip_range_blocklist`/`allowlist`, `verify_certificates`,
  `client_timeout`, `max_retry_backoff`, `allow_public_rooms_over_federation`,
  `allow_device_name_lookup_over_federation` — all validated), `hs-config::server::ServerConfig`
  (`server_name`, `signing_key_path` — a **directory**, multiple active keys expected during
  rotation, no parser for its on-disk format exists yet), `hs-cli::serve.rs` (confirms the router
  is built via `hs_http::router::Builder`, and that **no federation listener exists yet** —
  `hs-room` and `hs-media`'s routers are not mounted into `hs serve` either yet, only
  `hs-auth`'s; the "mount the way hs-room and hs-media mount theirs" instruction refers to the
  *pattern* in `crates/hs-media/src/router.rs` — a `Builder`-based router-fragment function
  returning `(Router<State>, RouteManifest)` for the listener to compose — not to an existing
  live mount point in `hs-cli` to copy), and `docs/synapse-inventory.md`'s federation route list
  (31 entries under `/_matrix/federation/*` plus two `_synapse/client/*` compat entries).
- Confirmed environment facts needed before writing code: network access to crates.io **is**
  available in this sandbox (checked directly); `hickory-resolver` (needed per the brief and
  decision 0007) is **not yet vendored** in the local cargo registry cache and is not yet in
  `[workspace.dependencies]` — it will need to be fetched and added (latest published: 0.26.3) by
  whoever picks this up next, noted in the root `Cargo.toml` per the workspace convention ("tracks
  add entries here only when missing, and record the addition in their status file" — not done
  yet, since no code was written). `reqwest` (workspace dep, rustls-tls, no default features) and
  `regex` (workspace dep, added by track 07) are both already available and are the intended
  building blocks for the client and ACL glob-matching respectively — no new dependency needed for
  those two. `libfuzzer-sys` and `arbitrary` are already vendored locally (found via
  `crates/hs-media/fuzz`, which is this track's template for fuzz-crate structure: a nested,
  separate-workspace `fuzz/` directory with its own `Cargo.toml` under `[package.metadata]
  cargo-fuzz = true`, one `[[bin]]` per target, and a `corpus/<target-name>/` directory with seed
  files — `crates/hs-media/fuzz/Cargo.toml` and its `fuzz_targets/*.rs` are the concrete template
  to copy the shape of).

## In progress

Nothing. The session was asked to stop starting new work before any file under `crates/
hs-federation/` was modified from its placeholder state (still just the `hs-federation: see
PLAN.md` doc-comment `lib.rs` it started with) or before `Cargo.toml` (crate or workspace) was
touched.

## Verification of the safe-stopping-point requirement

- `crates/hs-federation` is unmodified from the empty placeholder it started as.
- `cargo check -p hs-federation`: clean (trivially — no code).
- `cargo check --workspace`: clean.
- `cargo test -p hs-federation` / `cargo clippy -p hs-federation --all-targets -- -D warnings`:
  not run, since there is nothing to test or lint yet; both would trivially pass against the
  placeholder.
- **Inbound signature verification is not enforced anywhere, because no federation listener
  exists yet.** This is the single most important fact for whoever picks this up: there is
  currently zero federation attack surface exposed (no router is mounted anywhere), so there is
  nothing un-defended in production terms — but the very first commit of transport-server code
  must not expose a single route before the `X-Matrix` verification middleware is wired in front
  of all of them. See the design below.
- No fuzz targets exist yet (none can run without a nightly toolchain regardless; see the brief).
- No security-relevant code of any kind was written this session — the threat model document is
  the only artifact, and it is documentation, not an enforced check. Nothing here should be
  mistaken for "done."

## Next (the concrete plan for whoever resumes this, in the order the brief specifies)

1. **`Cargo.toml`**: add `hickory-resolver` (0.26.x) to `[workspace.dependencies]` in the root
   `Cargo.toml`, with a comment attributing the addition to track 06, per the convention every
   other track has followed (see the existing "Added by track NN" comments). Add
   `crates/hs-federation/Cargo.toml` dependencies: `hs-model`, `hs-kv`, `hs-tables`, `hs-http`,
   `hs-config` (workspace path deps), plus workspace deps `axum`, `tokio`, `reqwest`, `serde`,
   `serde_json`, `thiserror`, `tracing`, `base64`, `hex`, `sha2`, `rand`, `futures`, `http`,
   `bytes`, `hickory-resolver`, and `regex` (for ACL glob matching — already a workspace dep, no
   version to pick). Dev-dependencies: `hs-testkit`, `tokio` (test features already in the
   workspace `full` feature set), `tempfile`.
   - **Deliberately not a dependency**: `hs-room`. See "Decisions made" below — the
     `RoomDataSource` trait lives in `hs-federation` itself; the adapter onto `hs-room`'s real
     query surface is separate, later work (by this track or track 04, whichever owns the wiring
     once the state-engine gap closes), not a `Cargo.toml` addition to make now.
2. **`discovery.rs`**: implement the spec's server-name resolution algorithm behind three small
   traits (`WellKnownFetcher`, `SrvResolver`, `AddrResolver`) so the resolution *logic* is unit
   tested against fakes (no real DNS/network in tests) while `hickory-resolver`- and
   `reqwest`-backed implementations satisfy the traits for real use. This is what makes "the
   spec's own resolution test cases are the tests" achievable without network access in CI. The
   resolution order to implement (recalled from the spec's server-server API "Resolving server
   names" section and its worked examples table — re-verify against
   `refs/matrix-spec/content/server-server-api.md` if `refs/` has been fetched, or the published
   spec, before relying on memory for exact step ordering):
   1. IP literal (with or without port) → use directly, port defaults to 8448, no well-known/SRV.
   2. Hostname with explicit port → resolve via A/AAAA only, no well-known/SRV, using the literal
      port.
   3. Hostname, no port → fetch `https://<hostname>/.well-known/matrix/server` (GET, single
      request, no redirect-following — see threat model 2.1 on why not); on a valid
      `{"m.server": "delegated[:port]"}` body, resolve the delegated name using steps 1/2 as
      applicable (explicit port on the delegated name skips SRV; no port on the delegated name
      does an SRV lookup on the delegated host, `_matrix-fed._tcp` first then the deprecated
      `_matrix._tcp`, falling back to A/AAAA on port 8448 if neither SRV lookup returns records);
      on a missing/invalid well-known response, fall back to an SRV lookup on the **original**
      hostname (same `_matrix-fed._tcp` then `_matrix._tcp` order), falling back to A/AAAA on port
      8448 if neither returns records.
   - Caching: wrap `WellKnownFetcher` in a decorator that respects `Cache-Control: max-age`
     clamped to a sane range, with a fixed shorter TTL for cached failures — see the threat model's
     resource-limits table for the starting values this track picked (16 KiB body cap; exact
     min/default/max TTL clamp values were not finalized this session — pick and document them
     when writing the code, they were sketched but not committed to specific numbers here).
3. **`keys.rs`**: own-key loading from `ServerConfig::signing_key_path` (a directory; Synapse's
   on-disk line format `algorithm version base64_seed`, e.g. `ed25519 a_1 <base64>`, is the
   reference shape to parse — no parser exists anywhere in this codebase yet, this track owns
   writing it), first-boot key generation via `hs_model::signing::SigningKeyPair::generate` if the
   directory is empty, the `/_matrix/key/v2/server` response builder (self-signed via
   `hs_model::signing::sign_object`, reusing the event-signing machinery directly), the notary
   endpoints (`/_matrix/key/v2/query/{serverName}` and the batch POST form), a `RemoteKeyCache`
   (fetch a remote server's own `/_matrix/key/v2/server` directly — resolved via `discovery.rs` —
   verify self-signature before trusting anything in the response, cache bounded by
   `valid_until_ts`, track `old_verify_keys` separately per the threat model's rule: usable for
   verification only, never for asserting a key is still current), and in-flight-fetch
   de-duplication per origin (threat model 2.3's confused-deputy defence).
4. **`xmatrix.rs`**: `X-Matrix` header parse/build (`origin`, `destination`, `key`, `sig`, quoted
   values, tolerant of field order; `destination` required by current spec but the parser should
   be explicit about whether/how it tolerates its absence from older peers — a decision to make and
   record, not silently guess at), the exact signed-object shape (`method`, `uri`, `origin`,
   `destination`, `content` — omit `content` when there is no body), built on
   `hs_model::canonical`/`hs_model::signing` directly (do not reimplement canonicalization here).
   The axum middleware that verifies every inbound request **before any handler runs** is the
   single highest-priority piece of code in this whole track — see the threat model section 2.3
   and the "Decisions made" note below on how it must be wired so it cannot be bypassed per-route
   by accident.
5. **`acl.rs`**: `m.room.server_acl` evaluation (`allow`, `deny`, `allow_ip_literals`), one
   function used for both inbound accept and outbound send (threat model 2.7's explicit
   "exactly one place this logic can be wrong" requirement), glob-to-anchored-regex translation
   using the already-available `regex` crate, with tests asserting both match and *non*-match for
   adjacent patterns.
6. **`room_source.rs`**: the `RoomDataSource` trait (membership/visibility check for a server,
   event lookup, state lookup, auth-chain lookup, backfill, missing-events) plus an in-memory fake
   implementation for this crate's own handler tests. See "Decisions made" for why this exists
   instead of depending on `hs-room` directly.
7. **`destination_store.rs`**: a `DestinationStore` trait (get/record backoff state per
   destination) with an in-memory implementation and a real `hs-kv`/`hs-tables`-backed one (a
   `destinations` typed keyspace: destination → `{failure_count, retry_at, last_success_at}`) so
   "retry with backoff persisted ... so a restart resumes" is actually true, not aspirational.
8. **`client.rs`**: `FederationClient` over a shared `reqwest::Client` (HTTP/1.1 only to peers per
   the brief's stated recommendation — pick this as the default and record it as a decision if
   picked), per-destination `tokio::sync::Semaphore` (default concurrency 1, matching Synapse's
   default per the brief's open question), the allow/deny list and IP-range checks from
   `FederationConfig` applied to both the original server name and the resolved connection address
   (threat model 2.1/2.6), backoff-aware dispatch via `DestinationStore`. Test against
   `hs-testkit::FakeFederationPeer` for request-shape/retry behaviour (remembering it does not
   itself do X-Matrix verification, per the "Done" section above).
9. **`transport/` router**: mounted the way `crates/hs-media/src/router.rs` mounts its routes —
   a `Builder`-based function per surface returning `(Router<FederationState>, RouteManifest)` —
   with the X-Matrix verification middleware applied at the router-composition level (e.g. via
   `.layer()` on the whole merged federation router, not per-handler), so it is structurally
   impossible to add a new route to this router without it passing through verification first.
   Fully implement: `/version`, `/query/{queryType}` (profile, directory), `/user/devices/
   {userId}`, `/publicRooms`, `/hierarchy/{roomId}`, `/timestamp_to_event/{roomId}`,
   `/openid/userinfo`, `/event/{eventId}`, `/state/{roomId}`, `/state_ids/{roomId}`,
   `/event_auth/{roomId}/{eventId}`, `/backfill/{roomId}`, `/get_missing_events/{roomId}` — all
   against `RoomDataSource`, all through the resource limits in the threat model's table 3, all
   with membership/visibility checks per threat model 2.4. Mount the remaining
   `docs/synapse-inventory.md` federation entries as clearly-marked seams (verify signature, parse
   and bound-check the body, reject with a typed "not implemented" error, nothing else): `/send`,
   `/make_join`, `/send_join` (v1 and v2), `/make_leave`, `/send_leave` (v1 and v2), `/make_knock`,
   `/send_knock`, `/invite` (v1 and v2), `/exchange_third_party_invite`, `/3pid/onbind`,
   `/user/keys/claim`, `/user/keys/query` (owned jointly with track 08, which has not started),
   `/rooms/{roomId}/complexity`, `/extremities/{roomId}`, `/query/account_status`, and the two
   media endpoints (proxy to track 09, do not implement locally — leave a comment naming that
   owner). The two `_synapse/client/*` inventory entries are client-prefixed compat surface, not
   server-to-server; out of scope for this router, note the decision explicitly if skipping them.
10. **Fuzz targets** (`crates/hs-federation/fuzz/`, template: `crates/hs-media/fuzz/`): PDU JSON
    (drive `hs_model::event::Event::parse` directly — this crate does not need its own PDU parser,
    reuse track 02's), EDU JSON, the `X-Matrix` header parser, `.well-known` response bodies, and
    key-server response bodies. Seed corpora: at least one valid example and one adversarial
    example per target (oversized, malformed JSON, wrong types), matching
    `crates/hs-media/fuzz/corpus/*`'s pattern.

## Blockers

None that block *starting* — every interface this track's day-one work needs (`hs-model`,
`hs-http`, `hs-config`, `hs-kv`/`hs-tables`) is landed and stable. The one real dependency is
sequencing, not a blocker: inbound event *persistence* (`Command::PersistInbound`) and any
`RoomDataSource` adapter onto real `hs-room` data both wait on track 04's state-engine wiring
gap closing, per this track's own instructions — everything in "Next" above is scoped to not
need that.

## Interfaces provided

None yet — nothing has shipped. Once the plan above lands, this section should list: the
`FederationClient` API (for tracks 08, 09, 11 per the brief), the `RoomDataSource` trait (for
whoever writes the `hs-room` adapter), and the federation router-fragment function (for `hs-cli`
to mount).

## Interfaces needed

- **Track 04**: once the room actor's state-engine wiring lands (the concurrent gap named in this
  track's own instructions), a real `RoomDataSource` adapter over `RoomActorHandle` needs to be
  written — either by this track or by 04, whichever is better positioned when the time comes. The
  concrete hooks to build it on are already identified above (`query<T, F>`, `state_event`,
  `full_state`, `event_by_id`, `paginate`, `relations_of`).
- **Track 08 (E2EE)**: `/user/keys/claim` and `/user/keys/query` are explicitly joint-owned per
  the brief; this track will mount them as seams until 08 exists to define the real contract.

## Decisions made

- **`RoomDataSource` is a trait owned by `hs-federation`, not a dependency on `hs-room`.** Every
  per-room federation read endpoint needs to ask "is this room visible to this server, what does
  it contain" — in the finished system that's answered by the room actor, but this track's
  sequencing instructions explicitly say not to build inbound event processing on the room actor's
  current flat state map (`docs/rfcs/0010`'s gap 1: that map is only correct for the
  single-writer, no-fork case, which inbound federation immediately violates). The same reasoning
  extends to reads: a federation handler that silently degrades to "whatever the flat map
  currently holds" is not a defensible foundation for the threat model's per-room defences.
  Defining the trait in `hs-federation` keeps every handler's logic testable today against a fake,
  without taking a `Cargo.toml` dependency on a crate under heavy concurrent change whose
  query surface this track does not want to be coupled to before the state-engine wiring settles.
  The concrete adapter onto `hs-room`'s real query methods is later work, not blocked on anything
  from this track's own side.
- **No federation route is exposed before `X-Matrix` verification wraps the whole router**, not
  per-handler. This is a structural decision, not just a convention: the verification middleware
  should be applied once, at router-composition time, over the merged federation router
  (`crates/hs-media/src/router.rs`'s `Builder` pattern makes this natural — build the route table,
  then `.layer()` the whole thing), so a future route added to this crate cannot accidentally ship
  unauthenticated by omission. Recorded here so whoever writes `transport/mod.rs` does not
  restructure this into a per-handler extractor instead, which would reintroduce exactly the
  bypass-by-omission risk the threat model's section 2.3 calls out.
- **Join/leave/knock/invite handshakes and `/send` ship as seams this pass**, per explicit
  instruction and because `PLAN.md`'s own risk callout ("unbounded state in `send_join`") names
  this as the highest-risk surface in the whole track; building it against a state engine that
  isn't wired into room persistence yet would mean shipping either half-correct logic or logic
  that has to be thrown away once track 04 lands the real wiring. The threat model was written for
  these endpoints anyway (section 2.5) so the eventual implementation has a specification to build
  against rather than starting from nothing.
- **`hickory-resolver` is the DNS resolver** (per the brief and decision 0007's reuse principle),
  not a hand-rolled DNS client — not yet added to `Cargo.toml` this session (see "Next" item 1).
- **HTTP/1.1-only to federation peers** is the brief's own stated recommendation for one of its
  "open questions to settle first"; leaning towards adopting it as the default rather than
  negotiating HTTP/2, since peer behaviour on HTTP/2 across the real federation is inconsistent
  and HTTP/1.1 is the documented safe default — not yet committed in code, flagged here so
  whoever writes `client.rs` either confirms this or explicitly overrides it with a recorded
  reason.

## Reuse considered (decision 0007)

- **`hickory-resolver`**: adopted per the brief and decision 0007's own text (it names
  `hickory-resolver` explicitly as something the project already reuses for federation discovery).
  Not yet added to `Cargo.toml` — see "Next" item 1. No alternative considered; this was specified
  by name.
- **`reqwest`**: adopted, already a workspace dependency (rustls-tls backend, matching decision
  0007's "rustls for the HTTP and TLS stack" line) and already used the same way by `hs-appservice`,
  `hs-cli`, `hs-media`, `hs-modules`. No reason to hand-roll an HTTP client on top of `hyper`
  directly when every other outbound-HTTP track already standardized on `reqwest`.
- **Canonical JSON, hashing, event signing**: not reimplemented — `hs-model::canonical`/`hash`/
  `signing` already exist, are cross-checked against `ruma_signatures`, and are the correct home
  for this logic (it is genuinely the same algorithm for events and for request signing, per the
  spec, so one implementation serving both is the right call, not a missed reuse opportunity for
  this track to fill separately).
- **`ruma-federation-api`** (part of the workspace's `ruma` meta-crate, feature `federation-api`,
  confirmed present at `ruma-federation-api-0.14.0` in the local registry cache): considered for
  providing typed request/response structs for every federation endpoint rather than hand-writing
  JSON shapes. **Not adopted for the axum handler layer** in the plan above, for a concrete
  reason: `ruma-federation-api`'s types are built around the `ruma_common::api` `OutgoingRequest`/
  `IncomingRequest` conversion machinery (`http::Request`/`http::Response` round-tripping), which
  is a different integration seam than `hs-http::router::Builder`'s axum-extractor-based handlers
  that every other listener in this codebase already uses (`hs-auth::routes`, `hs-media::router`).
  Forcing the two together would mean either writing adapter code whose only job is bridging one
  request-typing convention to another (a maintenance burden decision 0007 asks tracks to avoid
  taking on without a clear win), or abandoning the `Builder`/`RouteManifest` convention this
  codebase has standardized on for every other listener, which is a bigger inconsistency than the
  JSON-shape duplication it would save. This should be revisited if a future pass finds the
  hand-written JSON shapes drifting from the spec in practice — `ruma-federation-api`'s types
  remain available as a cross-check reference (parse a handler's own response through Ruma's type
  in a test, the same way `hs-model` cross-checks against `ruma_signatures`) even without using
  them as the request/response binding layer itself. Not a closed question; recorded so the next
  session doesn't have to re-derive the tradeoff from scratch, and can revisit it if the
  hand-written-shapes cost turns out higher than expected.
- **DNS-over-the-wire parsing, TLS, and JSON parsing**: all reused (`hickory-resolver`, `rustls`
  via `reqwest`'s `rustls-tls` feature, `serde_json`) rather than reimplemented, per decision
  0007's default posture — no case was found in this session's research where a maintained,
  compatibly-licensed implementation was missing or unsuitable for this track's needs.

## Shared dependencies added

None yet. `hickory-resolver` (0.26.3, confirmed available on crates.io, not yet vendored locally)
is the one addition this track expects to make to `[workspace.dependencies]`; not added this
session (no code was written that needed it compiled). `regex` and `reqwest` are needed but
already present in the workspace from tracks 07 and (multiple), so no addition needed for those.
