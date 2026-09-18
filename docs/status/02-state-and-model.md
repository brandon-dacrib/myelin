# 02 State and model: status

Updated: 2026-09-18.

## Done

- **`hs-model`** (week-2 seam, frozen): all modules written and wired into `lib.rs`, replacing the
  `TODO` placeholders left from the interrupted attempt.
  - `canonical.rs`: canonical JSON (`CanonicalJsonValue`, `to_canonical_value`), strict-mode
    integer/float rules, spec appendix vectors, cross-checked byte-for-byte against
    `ruma_common::CanonicalJsonValue`.
  - `room_version.rs`: the capability table for room versions 1-12 plus the six unstable
    identifiers Synapse 1.161.0 advertises (`docs/synapse-inventory.md`), re-expressed from
    `ruma_common::room_version_rules` with attribution; field-for-field cross-checked against
    Ruma's own table for all twelve stable versions.
  - `hash.rs`: content hash and reference hash, spec vector (`5jM4wQpv6lnBo7CLIghJuHdW+s2CMBJPUOGOC89ncos`),
    cross-checked against `ruma_signatures`.
  - `redaction.rs`: the redaction algorithm per version, cross-checked against
    `ruma_common::canonical_json::redaction::redact` across five room versions and eight event
    shapes.
  - `event.rs`: `Event`/`EventHeader`/`EventFlags` (rejected, soft-failed, redacted, outlier,
    partial-state, packed into one byte), cached canonical bytes, version-aware event-ID handling
    (explicit for v1/v2, derived from the reference hash for v3+).
  - `power_levels.rs`: version-aware `PowerLevels` parsing (integer-only from v10, lenient
    string/float before) and `EffectivePowerLevels` (creator power, MSC4289, from v12).
  - `signing.rs`: ed25519 sign/verify over this crate's own canonical bytes, cross-checked against
    `ruma_signatures::verify_json`.
  - 50 tests, `cargo clippy -p hs-model --all-targets -- -D warnings` clean.

- **`hs-state`**, day-one work:
  - `auth.rs`: event authorization for room versions 1-12, one implementation parameterized by
    `RoomVersionRules` (not twelve copies), covering `m.room.create`, the auth-events-selection
    check, `m.room.member` (join/invite/leave/ban/knock, restricted and knock-restricted joins,
    third-party invites with real ed25519 signature verification), `m.room.power_levels`, and the
    pre-v3 `m.room.redaction` special case. 22 unit tests plus a 512-case property test
    (`tests/ruma_cross_check.rs`) cross-checking against `ruma_state_res::event_auth` across 12
    room-version/join-rule combinations and 6 random event shapes.
  - `state_res/`: `v1.rs` (in-house, from the room-version-1 spec text), `v2.rs` (wraps
    `ruma-state-res` for v2/v2.1), `oracle.rs` (independent v2/v2.1 implementation from the spec
    text, `#[cfg(test)]`-only). A 256-case property test (`state_res::cross_check_tests`) builds
    random two-fork rooms (realistic `auth_events` computed via `auth::expected_auth_types`) and
    checks `oracle::resolve` agrees with `v2::resolve` (the `ruma-state-res`-backed path) across 4
    room versions.
  - `chain_cover.rs`: the chain-cover auth index in interned form (`EventSn`/`StateKeyId`), with
    `coverage`, `contains` and `auth_chain_difference`. 6 tests including two 256-case property
    tests cross-checking against a brute-force transitive closure over random DAGs (found and
    fixed a real bug: two events citing the same ancestor as "the next version" could collide on
    one chain position before the fix added tip-tracking).
  - `api.rs`: the frozen `StateStore` trait (`state_at`, `get`, `diff`, `apply`, `resolve`,
    `chain_position`, `auth_chain_contains`, `auth_chain_difference`) with extensive rustdoc,
    including an explicit, justified decision that `state_at` returns *S′(event)* (state after)
    rather than *S(event)* (state used to authorize it) -- see the module docs for why and how a
    caller gets the other one.
  - `store.rs`: `InMemoryStateStore`, a reference `StateStore` implementation (not one of the
    section-6.3 bake-off candidates -- a plain `Vec` of maps) that tracks 04/06 can build against
    today; exercises the full trait including a real fork-and-merge scenario through
    `add_event`/`resolve`.
  - 37 unit tests plus the 1 integration test above, `cargo clippy -p hs-state --all-targets -- -D
    warnings` clean.

## In progress

- Nothing mid-flight; the above is all committed to a stable, tested state.

## Next

- Event auth for restricted/knock-restricted joins and third-party invites has direct unit
  coverage but not yet property-test coverage the way the main auth cross-check does (the
  `ruma_cross_check` fixture's baseline room doesn't exercise those join rules); worth extending.
- `state_res::v1` has no cross-implementation partner (`ruma-state-res` does not implement v1) --
  its 3 tests are scenario-based, not property-based against an oracle. A v1 oracle could still be
  written from the same spec text as a second check, if a future pass has budget.
- `hs-state`'s auth chain difference (`chain_cover.rs`) is not yet wired into `state_res::v2`'s or
  `state_res::oracle`'s own (currently brute-force) auth-chain computation; doing so is the actual
  performance win `PLAN.md` section 6.4 describes and belongs with whichever track next needs that
  performance (a Phase 1/2 concern per the brief, not Phase 0 correctness).
- Section 6.3's bake-off (three state representations behind `StateStore`, the synthetic corpus
  generators, the measurement harness, published scoring weights, the decision report) was **not
  attempted** -- explicitly the "if budget remains" tail of this assignment, and the remaining
  scope (three persistent data structures, a multi-corpus benchmark harness, and the report) is
  itself comparable in size to everything above. `InMemoryStateStore` is a head start (it proves
  the trait boundary works and gives 04/06 something to build against) but is not a bake-off
  candidate.
- MSC4242 edge tables (`prev_state_events`) and the v2.1 "conflicted state subgraph" are
  implemented in `state_res::oracle` (best-effort: events touched, not full per-pair-path
  enumeration) and in `state_res::v2`'s callback (conservative under-approximation: the conflicted
  candidates themselves, unexpanded) but not otherwise surfaced; both are documented Phase 1/2
  gaps in the code.
- Chain-cover rebuild-from-scratch (for an imported room) and background verification are Phase
  1/2 per the brief; not started.

## Blockers

- None. `hs-kv`/`hs-tables` (01) are not consumed yet; `hs-state`'s `EventStore`/`StateMap` types
  and `InMemoryStateStore`'s local interning are self-contained stand-ins, by design (see
  `state_res/mod.rs` and `store.rs` module docs), so track 02 was not blocked waiting on them.

## Interfaces provided

- **Week 2** (`hs-model`): event and identifier types, room-version capability table. Frozen; see
  `docs/status/02-state-and-model.md`'s "Done" above for what landed.
- **Week 6** (`hs-state`): the `StateStore` trait (`crates/hs-state/src/api.rs`) -- `state_at`,
  `diff`, `apply`, `resolve`, chain-cover queries. Frozen; tracks 04 and 06 build against it. A
  reference implementation (`InMemoryStateStore`) exists today so 04/06 do not need to wait for
  the bake-off decision to start integrating.
- `hs_state::auth::{check_auth_events_selection, check_event_auth}` and the `StateFetch` trait
  (`crates/hs-state/src/state_fetch.rs`): usable independently of `StateStore` by anything that
  already has a state snapshot in some other form (e.g. a federation `send_join`/`send_leave`
  handler validating a remote server's claimed state before trusting it).

## Interfaces needed

- 01 (`hs-kv`/`hs-tables`): the real interning API, once it lands, should replace
  `InMemoryStateStore`'s local `key_of`/`event_id_of` maps; no interface change to `StateStore`
  itself is expected.
- 04 (room actor): tell track 02 which of `StateStore`'s methods the room actor calls in which
  order (particularly whether it wants `state_at` of *S(E)* or *S′(E)* more often) so the trait's
  ergonomics can be revisited before the interface freeze is final, if needed.
- 14 (test and conformance): the brief's definition of done names "Synapse's own implementation
  driven through the Python harness that 14 provides" as a fourth cross-check partner for state
  resolution; that harness does not exist yet from this track's side.

## Decisions made

- **Canonical JSON leniency is parameterized by `strict: bool`, not by room version directly**
  (`hs-model/src/canonical.rs`): room versions 1-5 (`strict_canonical_json = false`) get
  `CanonicalJsonValue::Float` instead of a hard error for non-conforming numbers, matching what
  those room versions' authorization rules actually require; the wire encoding of that lenient
  case is documented as best-effort, not guaranteed to match other implementations, which the spec
  itself allows for pre-v6 rooms.
- **The unstable Synapse room versions** (`org.matrix.hydra.11` and five others) are modeled as
  inheriting a named stable version's full rule set plus `disposition: Unstable`, rather than
  independently reverse-engineered from their MSCs (`hs-model/src/room_version.rs`, `KNOWN`
  table's doc comment). This is a best-effort placeholder pending each MSC's own text; anyone
  implementing one of those MSCs for real should revisit that table entry specifically.
- **`state_at(event)` returns *S′(event)* (state after), not *S(event)* (state used to authorize
  it)** (`hs-state/src/api.rs`): see that module's "`state_at` returns the state *after* the
  event" section for the full rationale; recorded here because it is exactly the kind of
  interface-freeze decision other tracks need to know about, not discover by reading the source.
- **`StateKeyId` is the only key type `StateDiff`/`StateStore::get` use**, matching `PLAN.md`
  section 6.1's "Conduit and Palpo trick" (`(event_type, state_key)` interned as one ID) rather
  than a `(TypeId, StateKeyId)` pair.
- **The chain-cover index's chain-extension heuristic** (`hs-state/src/chain_cover.rs`,
  `add_event`'s doc comment): prefer an auth event with the same interned key, else the deepest
  currently-extendable ("tip") known auth event, else start a new chain. This is a documented,
  reasonable construction, not a reproduction of Synapse's own (AGPL, read for the general idea
  only, not copied); it was property-tested against brute-force transitive closure over random
  DAGs (`chain_cover::property_tests`) and a real bug (position collisions on forked histories)
  was found and fixed by adding per-chain tip tracking.
- **Third-party invite signature verification supports only ed25519** (`hs-state/src/auth.rs`,
  `verify_third_party_signature`'s doc comment): the only algorithm identity servers use in
  practice; other `signed.signatures` key algorithms are silently skipped rather than erroring
  (matching the spec's "if any signature ... matches any public key ..., allow" -- a non-matching
  or unsupported signature is just not a match).
- **`hs-state`'s `EventStore`/`StateMap`/`ResolutionEvent` types are deliberately not the interned,
  production representation** (`hs-state/src/state_res/mod.rs` module docs): they are the
  self-contained, owned representation the three state-resolution algorithms and their
  cross-checks operate on; `InMemoryStateStore` bridges them to the interned `StateStore` API
  locally. This kept track 02 unblocked on track 01's interning API landing.

## Shared dependencies added

Noted in `[workspace.dependencies]` in the root `Cargo.toml` (added by this track):
- `rand_core = "0.6"`: the version `ed25519-dalek` 2.x's optional `rand_core` feature pins (needed
  for `SigningKeyPair::generate` in `hs-model::signing`). Unrelated to and does not replace `rand`
  (0.9), which stays for everything else.

Crate-local additions (already-shared versions, `workspace = true`): `hs-model` added
`ed25519-dalek` (with the `rand_core` feature) and `rand_core`; `hs-state` added `ed25519-dalek`,
`base64`, `sha1`, and dev-dependencies `proptest`, `rand`.

## Verify

```
cargo fmt --check -p hs-model -p hs-state
cargo clippy -p hs-model -p hs-state --all-targets -- -D warnings
cargo test -p hs-model -p hs-state
```

All green as of this update: 50 tests in `hs-model`, 37 unit tests plus a 512-case property test
in `hs-state`.
