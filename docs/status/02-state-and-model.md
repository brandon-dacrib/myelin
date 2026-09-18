# 02 State and model: status

Updated: 2026-09-18 (session 3).

## Session 3 (this session): closing the room-actor/state-store seam

Assignment: close the gap `docs/rfcs/0010-room-actor-state-store-seam.md` (written by track 04)
named, promote the bake-off winner into production, and make the production store usable by the
room actor. Summary of what landed, in the order the RFC asked for; full detail in the sections
below (grep for "Session 3").

1. **RFC gap 1, closed**: `StateStore::intern_state_key(&self, event_type: &str, state_key: &str)
   -> Result<StateKeyId, Self::Error>` is now on the frozen trait (`crates/hs-state/src/api.rs`),
   implemented by both `InMemoryStateStore` and the new production store. This is the RFC's
   "preferred" option (option 1): a thin wrapper around each store's existing internal
   get-or-create interning, made reachable from outside the module. Get-or-create is idempotent:
   calling it before or after ingesting an event that sets that key returns the same id either
   way, because both paths share one table.
2. **Bake-off winner promoted to production**: candidate B moved out of `crate::bakeoff` into
   `crate::frames` (`FrameRepr`) and `crate::kv_store` (`KvStateStore`, `ProductionStateStore<KV>`
   type alias, `KvStateStore::open` convenience constructor). Candidates A and C stay under
   `crate::bakeoff`, explicitly marked benchmark-only in that module's doc comment, kept (not
   deleted) for `docs/decisions/0006-state-bakeoff-results.md`'s "What would change this decision"
   re-runs. `InMemoryStateStore` (`crate::store`) is unchanged and still what tests use.
3. **Partly done**: the production store's primitives (`intern_state_key`, `get`, `apply`,
   `resolve`, and a new default `current_state` convenience method that resolves a room's forward
   extremities) are all there and tested. **Not done**: an adapter from `(StateStore, Root)` to
   `state_fetch::StateFetch` that would let track 04 delete its flat map entirely. See "Next".
4. **Not done**: the fork-resolution integration test cross-checking the production store against
   both `state_res::v2` and `state_res::oracle` on the same conflicting state maps. See "Next" --
   recorded honestly as incomplete, not glossed over.
5. This status file, updated (this pass).

`cargo check -p hs-state`, `cargo test -p hs-state -p hs-model` (59 + 50 tests, including the
`ruma_cross_check` property test and `state_res::cross_check_tests`'s oracle-vs-ruma property
test), and `cargo clippy -p hs-model -p hs-state --all-targets -- -D warnings` are all clean as of
this update. `cargo check -p hs-room` (the trait's only other consumer in the workspace) and
`cargo check --workspace` were also run to confirm the trait change (`intern_state_key` is a new
*required* method) does not break anything outside this crate -- nothing else in the workspace
implements `StateStore`, so nothing else needed updating. (`cargo check --workspace` fails on an
unrelated, pre-existing error in `hs-appservice`, another track's concurrent work, not touched by
or related to this session.)

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

- **`PLAN.md` section 6.3's state-representation bake-off**, the deliverable this track's brief
  deferred ("if budget remains") and the previous status update flagged as not attempted. Fully
  delivered in this pass: weights and methodology published before any measurement
  (`docs/decisions/0005-state-bakeoff-methodology.md`), all three candidates implemented, real
  corpus generators, a measurement harness, a full run on both `hs-kv` backends, and a results
  document with real numbers, a weighted score and an explicit decision
  (`docs/decisions/0006-state-bakeoff-results.md`). Summary: **candidate B (deduplicated frames)
  wins the weighted score (0.819 vs. A's 0.766 vs. C's 0.513), a 5.3-point margin over A** -- real
  but thin; the results document is explicit that A is highly competitive and that this is not a
  closed question (see "What would change this decision" there).
  - `crates/hs-state/src/bakeoff/repr.rs`: the `StateRepr` trait (get/full_state/diff/apply on an
    opaque `Root`) and `BakeoffStats` (bytes-on-disk/bytes-written/compaction-events/dedup-hits),
    the narrow interfaces each candidate implements so `generic_store.rs` can share one tested
    ingestion/resolution implementation across all three (not three near-copies of
    `InMemoryStateStore`'s `add_event`/`resolve_locked`).
  - `crates/hs-state/src/bakeoff/generic_store.rs`: `GenericStore<R: StateRepr>`, a full
    `StateStore` for any candidate. Its `resolve_locked` bases a resolved fork's diff on one input
    fork (not the empty state) specifically so structural sharing survives resolution -- an early
    version rebuilt the whole state from scratch on every resolve, which would have silently
    erased candidates B's and C's sharing advantage before the bake-off ever measured it. 3 tests
    replay the same fork-and-merge scenario `store.rs`'s own test uses, through all three
    candidates, as the correctness gate the bake-off's numbers depend on.
  - `crates/hs-state/src/bakeoff/snapshot_delta.rs` (candidate A): a state-group id per change,
    delta from parent, full snapshot every `SNAPSHOT_INTERVAL` (50, bake-off scale) hops; `diff`
    has a same-chain fast path and a full-materialization fallback for anything else (forks,
    non-adjacent states) -- deliberately reproducing the "every lookup chases the chain, diff
    between arbitrary states walks layers" cost model `PLAN.md` names as this candidate's known
    failure mode, so the bake-off could actually observe it (and did -- see the results document's
    `moderation_policy_room` full-history diff number).
  - `crates/hs-state/src/bakeoff/frames.rs` (candidate B): content-hash-keyed frames
    (`(parent, appended, disposed)` hashed with SHA-1, truncated to 128 bits), delta-varint
    compressed appended/disposed lists (`bakeoff::varint`), periodic full "base" frames every
    `REBASE_INTERVAL` (50) hops. Content-hash keying gives free dedup when an identical delta from
    an identical parent recurs (measured: 174 hits on the fork scenario, 0 elsewhere in this
    corpus -- an honest finding, not the space-saving story `PLAN.md`'s "dedup is automatic"
    implies for ordinary traffic; B's actual disk-size win in this run comes from varint
    compression, not hash collisions).
  - `crates/hs-state/src/bakeoff/persistent_map.rs` (candidate C): a 32-way trie over
    `StateKeyId`'s 32 bits (7 levels, single-entry subtrees inlined as leaves), nodes stored and
    deduplicated by content hash, `diff` short-circuiting the instant two subtree hashes match.
    `apply`'s node writes for one logical state change are staged into one `WriteBatch` and
    committed in a single transaction (`store_staged`/`commit_batch`) rather than one transaction
    per trie level -- an early version committed per-level and was roughly 7x slower for no
    correctness benefit, found by timing during this pass, not by inspection. `gc()` is a real
    mark-and-sweep pass (batched into `MAX_TXN_MUTATIONS`-sized chunks after this pass discovered
    `hs_kv`'s 10,000-mutation-per-transaction limit the hard way on the `large_room_membership_churn`
    and `moderation_policy_room` scenarios), since this candidate's path-compression-on-delete
    leaves real garbage that A's and B's inline compaction never does. A 3,000-op randomized test
    against a `BTreeMap` oracle plus a fork/diff/gc test suite back this implementation; see "Why C
    lost" in the results document for why this candidate's specific 7-levels-per-key design, not
    content-addressed persistent maps as an idea, is most of why it lost this run.
  - `crates/hs-state/corpus/generators.rs` (via `#[path]` from `src/lib.rs`, module `corpus`):
    synthetic generators for all five `PLAN.md` section 6.3 corpus scenarios, each documenting its
    own scale-down factor from the number `PLAN.md` names (no real-room corpus was reachable --
    no network, no throwaway server to join from; see the methodology document's "known, declared
    limitations").
  - `crates/hs-state/src/bin/bakeoff.rs` + `crates/hs-state/corpus/run_bakeoff.sh`: the measurement
    harness (one candidate/backend/scenario combination per process invocation, JSON metrics to
    stdout) and the driver script that runs all 30 combinations under `/usr/bin/time -l` for
    isolated resident-memory readings and assembles
    `crates/hs-state/corpus/results/bakeoff-results.jsonl`, the results document's raw data.
  - 27 new tests across the five `bakeoff` modules and `corpus` (20 unit tests in the candidate
    modules plus the 3-candidate `generic_store` correctness gate, plus 2 corpus well-formedness
    tests, plus 2 varint tests), all passing; `cargo clippy -p hs-state --all-targets -- -D
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
- ~~The bake-off's winner (candidate B) is not yet wired up as the *default* production
  `StateStore`~~ **Done this session** -- see "Session 3" below. `crate::kv_store::ProductionStateStore<KV>`
  (`KvStateStore<FrameRepr<KV>>`) is now the production `StateStore`; `InMemoryStateStore` remains
  for tests only.
- **Not done this session, and the most important remaining gap**: `hs-state` still has no adapter
  that turns a `(StateStore, Root)` pair plus an event-body source into
  `state_fetch::StateFetch`, so track 04 cannot yet delete `crate::pipeline::CurrentState`'s flat
  map and hand auth checks a `StateStore`-backed view directly. `intern_state_key` (below) plus
  `get`/`apply`/`resolve`/`current_state` are the primitives; the glue that turns them into
  something `hs_state::auth` can call still needs writing. See "Next" for the concrete shape.
- **Not done this session**: the integration test proving a genuine fork (two events extending one
  parent with conflicting state) resolves identically through the production store,
  `state_res::v2` (ruma-state-res-backed) and `state_res::oracle` (the independent implementation)
  all three. `crate::kv_store`'s own `tests` module exercises fork-and-merge through the production
  representation already (see `kv_store::tests::candidate_b_frames`) and matches the same fixed
  expected winner `store.rs`'s and `generic_store`'s tests always have, but nothing yet calls
  `state_res::oracle::resolve` and `state_res::v2::resolve` directly on the *same* two conflicting
  state maps used to drive the production store and asserts the three-way agreement explicitly.
  This is real, not yet closed, evidence work -- flagged clearly rather than claimed done.
- The results document's "What would change this decision" section names three concrete follow-ups
  if budget allows before this is treated as closed: (1) a real-room corpus once network access and
  a throwaway server are available, (2) a wider-fan-out or coarser-grain revision of candidate C
  specifically (its loss this run is substantially explained by a 7-levels-per-key design choice,
  not the content-addressed-persistent-map idea itself -- see the results document's "Why C lost"),
  (3) re-running against PostgreSQL or SlateDB once either exists (`PLAN.md` section 6.5), since
  network round-trip cost changes the relative penalty of B's one-record-per-write versus C's
  up-to-seven differently than local Fjall does.
- MSC4242 edge tables (`prev_state_events`) and the v2.1 "conflicted state subgraph" are
  implemented in `state_res::oracle` (best-effort: events touched, not full per-pair-path
  enumeration) and in `state_res::v2`'s callback (conservative under-approximation: the conflicted
  candidates themselves, unexpanded) but not otherwise surfaced; both are documented Phase 1/2
  gaps in the code.
- Chain-cover rebuild-from-scratch (for an imported room) and background verification are Phase
  1/2 per the brief; not started.

### Next, in detail: the two items session 3 left open (concrete, for whoever picks this up)

1. **A `StateFetch` adapter over `StateStore`.** Add (likely in `crate::state_fetch`, since it
   already owns the trait) a type like `StoreStateFetch<'a, S: StateStore, E>` that holds a
   `&'a S`, an `S::Root`, and something that maps `EventSn -> (&UserId, &CanonicalJsonObject)`
   (event bodies -- the room actor already has these in memory; a small local trait, analogous to
   `state_fetch::FlatState`'s shape, is probably the cleanest way to keep this generic instead of
   depending on `hs_model::event::Event` directly). `get(event_type, state_key)` becomes
   `self.store.intern_state_key(event_type, state_key)` then `self.store.get(self.root, key_id)`
   then a body lookup. Once this exists, `crate::pipeline::CurrentState`
   (`crates/hs-room/src/pipeline.rs`) can be replaced by this type plus a `ProductionStateStore`
   held per room, and `RoomActor::current_state`'s flat `BTreeMap` can go away. This is track 04's
   work once the adapter exists (do not edit `hs-room` from this track), but the adapter itself is
   this track's to build.
2. **The three-way fork integration test.** Build a small room (create, join, power_levels, two
   branches setting the same key to conflicting values, e.g. `m.room.name`) as both (a) a sequence
   of `ProductionStateStore::add_event` calls ending in a merge event that triggers `resolve()`
   internally, and (b) two `state_res::StateMap`s over one shared `state_res::EventStore` built
   from the same events by hand (`state_res::cross_check_tests::RoomBuilder` is the existing
   pattern to reuse). Call `state_res::v2::resolve` and `state_res::oracle::resolve` on (b)
   directly, call `ProductionStateStore::get` on (a)'s merge root for the conflicting key, and
   assert all three name the same winning event. `state_res::oracle` is `#[cfg(test)] pub(crate)`
   (deliberately, from session 1 -- see "Decisions made" further down), so this test must live
   inside the crate (a `#[cfg(test)]` module, e.g. `crate::kv_store`'s existing `tests` module or a
   new sibling), not under `crates/hs-state/tests/` where it would not compile against `oracle` at
   all.
3. Everything in the pre-session-3 "Next" list below this point that session 3 did not touch
   (restricted/knock-restricted join property coverage, `state_res::v1`'s missing oracle partner,
   chain-cover-index wiring into `state_res`'s auth-chain computation, MSC4242 edge tables, and
   chain-cover rebuild-from-scratch) is all still open and still Phase 1/2 per the brief.

## Blockers

- None. `hs-kv`/`hs-tables` (01) are consumed by the production store (`crate::kv_store`,
  `crate::frames`, over `hs_kv::memory::MemoryBackend` and `hs_kv::fjall_backend::FjallBackend`)
  and by the two remaining benchmark-only candidates (`crates/hs-state/src/bakeoff/*.rs`, with
  `hs_tables::TupleKey` for candidate A's group-id keys); both crates were already stable and
  well-documented when this track needed them, so no wait was involved. `InMemoryStateStore`'s own
  local interning is unchanged and still self-contained by design (see `state_res/mod.rs` and
  `store.rs` module docs) -- it is a separate, test-only reference implementation, not something
  this pass touched.

## Implications for tracks 04 and 06

The bake-off's winner, now the production representation (`crates/hs-state/src/frames.rs`,
`FrameRepr`, wrapped as a `StateStore` by `crates/hs-state/src/kv_store.rs`'s
`ProductionStateStore<KV>`), is a linked chain of content-addressed frames: `state_at(event)` is a
16-byte hash, `get` walks the chain from that hash towards a periodic full "base" frame, `apply`
writes exactly one new frame per logical state change. What this means concretely for the two
consumers of `StateStore`:

- **Track 04 (room actor).** The room actor's dominant access pattern -- authorize the next event
  against the current room state, then advance `state_at` by one -- is exactly candidate B's cheap
  path: the ancestor-chain fast path in `diff` and the single-record `apply` both assume "one step
  forward from where you already are," and the results document's "diff, 1 apart" row (0.33-0.34
  µs for A and B, pooled) confirms that assumption holds cheaply for B specifically, not just for A.
  The room actor should **hold the frame hash it is building from, not recompute it**, exactly as
  `StateStore::apply`'s docs already say (`root` is unchanged, functional update) -- there is no
  cost model reason to ever re-derive a root from an event id when the actor already has the root
  in hand. The one place B is *not* cheap is a large fork: `apply`'s periodic re-basing
  (`REBASE_INTERVAL`, 50 hops in this bake-off, a value the room actor's real deployment should
  probably tune per room size rather than leave at the bake-off's default) means the actor
  occasionally pays a full-state-materialization cost on an otherwise ordinary event; this should
  show up as an occasional latency spike in the room actor's own metrics, not a bug, and track 04
  should expect and budget for it rather than be surprised by it.
- **Track 06 (federation).** `/state` and `/state_ids` at a `prev_events` boundary, and MSC4242's
  explicit-DAG state fetching, are exactly "diff between two states that are not adjacent" --
  candidate B's *without* structural-sharing shortcut path (full materialization on both sides when
  the two roots are not on the same chain), which the results document's
  `moderation_policy_room` full-history row shows costing 639-727 µs even for B in this corpus (a
  worst case: 8,000 distinct never-overwritten keys). Federation should **not** assume state
  diffing across an arbitrary pair of remote-claimed roots is cheap under the winning candidate;
  it is cheap only along the chain the local server actually walked to get there. A federation peer
  that is far behind (deep backfill, a long partition) is the scenario `fork_and_backfill`
  approximates, and there candidate B was competitive with A but **not** the standout candidate C
  was on the specific sub-case where most of the state is shared and only a little differs (see the
  results document's "deep backfill stand-in" row, 26.5 µs for B versus 0.041 µs for C) -- if
  federation's real workload turns out to be dominated by exactly that shared-mostly, differ-a-bit
  pattern (plausible for a server resuming after a short outage, less plausible for a server joining
  a room fresh), it is worth track 06 flagging that back to this track as a reason to revisit the
  bake-off's margin rather than treating candidate B as permanently settled.
- **Both tracks**: resolution time on forks (weighted highest in the bake-off alongside disk bytes)
  was A's best category, not B's, though B was close behind (1,475 µs vs. A's 1,436 µs p50, pooled
  from `fork_and_backfill`'s 25 merge events) -- close enough that neither track should expect a
  user-visible difference from this specific choice on that axis.

## Interfaces provided

- **Week 2** (`hs-model`): event and identifier types, room-version capability table. Frozen; see
  `docs/status/02-state-and-model.md`'s "Done" above for what landed.
- **Week 6** (`hs-state`): the `StateStore` trait (`crates/hs-state/src/api.rs`) -- `state_at`,
  `get`, `diff`, `apply`, `resolve`, `current_state` (new, session 3, see below), chain-cover
  queries, and `intern_state_key` (new, session 3). Frozen; tracks 04 and 06 build against it. A
  reference implementation (`InMemoryStateStore`) exists today for tests.
- **Session 3, current** (`hs-state`): **the production `StateStore` to actually hold, as of this
  update**: `hs_state::kv_store::ProductionStateStore<KV>` (`= KvStateStore<FrameRepr<KV>>`),
  constructed with `ProductionStateStore::open(room_version, backend)` for any `hs_kv::KvBackend`
  (`crates/hs-state/src/kv_store.rs`). This is the promoted bake-off winner
  (`docs/decisions/0006-state-bakeoff-results.md`), no longer under `bakeoff`. Its representation
  is `hs_state::frames::FrameRepr` (`crates/hs-state/src/frames.rs`) if you need the type directly
  (e.g. for `KvStateStore::new` with a hand-built `repr`); most callers want `open`, not `new`.
  `hs_state::bakeoff::{SnapshotDeltaRepr, PersistentMapRepr}` (candidates A and C) are
  benchmark-only now -- do not build on them.
  - **The exact call sequence a room actor should use** (replacing `CurrentState`'s flat map, once
    the `StateFetch` adapter in "Next" item 1 exists -- until then, everything below except the
    `StateFetch` step is usable as-is): on ingesting an event, call
    `store.add_event(...)` (mirrors `InMemoryStateStore::add_event`'s parameter list exactly --
    event id/sn, room id, event type, state key, sender, content, depth, timestamp, auth_events,
    prev_events, `only_prev_event_is_room_create`); this returns the new `state_at` root directly.
    To authorize the *next* event: if there is exactly one forward extremity, its `state_at` root
    is the auth state, no `resolve()` call needed (matches `StateStore::resolve`'s own "single
    fork is a no-op" documented behavior). If there is more than one forward extremity (the fork
    case this whole assignment exists for), call `store.current_state(room_version,
    &forward_extremity_event_sns)` -- the new default trait method that does `state_at` of each
    plus `resolve()` in one call -- to get the merged root. To look up one state entry (a specific
    user's membership, power levels, etc.) independent of already holding the event that set it:
    `let key = store.intern_state_key(event_type, state_key)?;` then `store.get(root, key)?`.
  - `intern_state_key`'s signature: `fn intern_state_key(&self, event_type: &str, state_key: &str)
    -> Result<StateKeyId, Self::Error>`. Idempotent get-or-create; safe to call before or after any
    event that sets that key exists in the store, and always returns the same id both times.
- `hs_state::auth::{check_auth_events_selection, check_event_auth}` and the `StateFetch` trait
  (`crates/hs-state/src/state_fetch.rs`): usable independently of `StateStore` by anything that
  already has a state snapshot in some other form (e.g. a federation `send_join`/`send_leave`
  handler validating a remote server's claimed state before trusting it). **No adapter from
  `StateStore` to `StateFetch` exists yet** -- see "Next" item 1; today a caller must still bridge
  the two by hand (look up via `intern_state_key`/`get`, then fetch the winning event's own
  sender/content from wherever it keeps event bodies).

## Interfaces needed

- 01 (`hs-kv`/`hs-tables`): the real interning API, once it lands, should replace
  `InMemoryStateStore`'s local `key_of`/`event_id_of` maps; no interface change to `StateStore`
  itself is expected.
- ~~04 (room actor): tell track 02 which of `StateStore`'s methods the room actor calls in which
  order~~ **Answered this session**: `docs/rfcs/0010-room-actor-state-store-seam.md` supplied
  exactly this, and "Interfaces provided" above now names the call sequence. What is still needed
  *from* track 02, for track 04 to actually rewire `crate::pipeline::CurrentState`: the
  `StateStore`-to-`StateFetch` adapter, "Next" item 1. Until it lands, track 04's flat map remains
  the pragmatic choice for the single-writer case (per the RFC's own section 4-equivalent
  reasoning), and this track is not asking track 04 to rewire early against half-finished plumbing.
- 06 (federation): once inbound `/send` transactions exist, `Command::PersistInbound`
  (`docs/design/04-room-actor-protocol.md` section 2) is where a fork first becomes real -- an
  inbound event whose `prev_events` do not include the local forward extremity. The call is
  `store.current_state(room_version, &all_current_forward_extremities)` after ingesting the new
  event via `add_event` (which itself may need to resolve if the new event's own `prev_events`
  already fork). No interface changes anticipated beyond the `StateFetch` adapter both 04 and 06
  will want.
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
- **The bake-off scoring weights were published in `docs/decisions/0005-state-bakeoff-methodology.md`
  before any candidate was measured**, per the task's explicit requirement that the decision not be
  rationalized after the fact; the file's own changelog note says any future re-run must edit the
  weights first, then re-run, not the reverse.
- **The bake-off's shared ingestion/resolution logic bases a resolved fork's diff on one input
  fork, not the empty state** (`bakeoff::generic_store::GenericStore::resolve_locked`): applying
  the full resolved map from empty every time would have measured "rebuild everything from
  scratch" instead of each candidate's actual structural-sharing behavior, silently erasing
  exactly the property (B's and C's) the bake-off exists to compare.
- **Candidate C's node writes for one `apply` call are batched into a single transaction**
  (`bakeoff::persistent_map`'s `WriteBatch`/`store_staged`/`commit_batch`), not committed per trie
  level: found necessary by timing during this pass (roughly 7x faster after the change on
  `hs_kv::memory::MemoryBackend`, whose per-commit cost is `O(keyspace size)` and punishes extra
  commits especially hard), and it also makes one `apply` call properly atomic, matching `hs_kv`'s
  own guidance to keep one transaction to one logical unit of work.
- **Candidate C's `gc()` deletes in `MAX_TXN_MUTATIONS`-sized batches, not one transaction**:
  found the hard way (`KvError::TransactionTooLarge`) on the `large_room_membership_churn` and
  `moderation_policy_room` scenarios, where a single-live-root garbage-collection pass produced
  more than `hs_kv::MAX_TXN_MUTATIONS` (10,000) deletions. This is itself part of what candidate
  C's GC costs in practice and is reported as such (a real, multi-commit operation) in the results
  document, not smoothed over.
- **MemoryBackend results were collected but excluded from the bake-off's weighted score**
  (`docs/decisions/0006-state-bakeoff-results.md`, "Why MemoryBackend is excluded from the
  score"): `hs_kv::memory::MemoryBackend` cloning its whole keyspace on every commit (documented in
  that crate as "not a performance backend") makes its numbers a measurement of the reference
  backend's own known limitation, not of the three candidates; scoring it would have been
  scoring `hs-kv`, not `hs-state`.

### Session 3 decisions

- **`intern_state_key` is a required trait method, not a default/provided one.** Both existing
  implementations (`InMemoryStateStore`, `KvStateStore`) already had a working get-or-create
  interning table internally; making the trait method required (rather than, say, a default that
  panics or returns "unsupported") costs nothing here and means a future third implementation
  cannot silently ship without it, which the RFC's whole complaint was about.
- **`current_state` (forward-extremity resolution) is a default-provided trait method, not
  required.** Unlike `intern_state_key`, every implementation gets this for free from `state_at` +
  `resolve`, both of which were already required; making it required too would only be a
  compile-time nuisance with no behavioral benefit, and a future implementation with a cheaper way
  to resolve straight from a set of extremities (skipping materializing each one's root first) can
  still override it.
- **Promoted names**: `bakeoff::generic_store::GenericStore` became `kv_store::KvStateStore`,
  `bakeoff::generic_store::BakeoffError` became `kv_store::KvStoreError`, and
  `bakeoff::repr::BakeoffStats` became `repr::ReprStats`. Reasoning: these three names are now
  either the production `StateStore`/error type track 04 and 06 will actually name in their own
  code, or (for `ReprStats`) a trait implemented by the production representation too, not just
  the two benchmark candidates -- keeping "Bakeoff" in a name that production code now
  instantiates would have been actively misleading to a reader who has never heard of the
  bake-off. `StateRepr` itself, `SnapshotDeltaRepr`, `PersistentMapRepr`, `RootA`, `RootC` and
  everything under `bakeoff::` were left alone: they are still exactly what they were, benchmark
  infrastructure and the two losing candidates.
- **Candidates A and C were kept, not deleted**, per this track's own call from session 2's "Next"
  ("kept clearly marked as benchmark-only or removed, your call, recorded either way" from this
  session's assignment). Both are still fully tested and wired into `crate::kv_store`'s own
  correctness-gate tests (`candidate_a_snapshot_delta`, `candidate_c_persistent_map`) alongside the
  production candidate, and `docs/decisions/0006-state-bakeoff-results.md`'s "What would change
  this decision" section names concrete conditions (a real-room corpus, a re-run against
  PostgreSQL/SlateDB) under which a re-run is plausible; deleting working, tested code that a
  documented future re-run might need did not seem cheaper than keeping it, per
  `docs/decisions/0007-build-less-reuse-more.md`'s framing of "reasonable" cutting both ways.
- **`InMemoryStateStore` was left as a hand-written reference implementation, not rebuilt on top of
  `KvStateStore`/`StateRepr`.** It predates this session, is fully tested, has no dependency on
  `hs-kv`, and nothing in this assignment required touching it; rebuilding it on the generic
  machinery would have been a pure refactor with no behavioral change and real risk of regressing
  its existing tests for no benefit this assignment asked for.

## Reuse considered

Per `docs/decisions/0007-build-less-reuse-more.md`'s standing obligation. This track's substantial
build decisions and why an existing project was not used instead:

- **State resolution v2/v2.1**: not reimplemented; `ruma-state-res` does the work
  (`crate::state_res::v2`), matching decision 0007's explicit list of what this project already
  reuses ("Ruma for ... state resolution v2 and v2.1"). The independent oracle
  (`crate::state_res::oracle`) is not a reimplementation-instead-of-reuse decision -- it is
  intentional redundancy for correctness evidence, the same reason this project runs Complement
  and Sytest instead of trusting one implementation's tests alone, and it stays test-only precisely
  so it never becomes a second production code path competing with `ruma-state-res`.
- **State resolution v1**: no maintained Rust implementation exists (`ruma-state-res` does not
  implement it; v1 is obsolete enough -- room versions 1-2 only -- that no other Matrix Rust
  project maintains one either, as far as this track found). Built in-house
  (`crate::state_res::v1`), from the spec text, the smallest version of the algorithm the project
  needs. This was decided in session 1 and is repeated here because decision 0007 asks every track
  to record this, not just the track that first made the call.
- **The state storage representation itself (`crate::frames::FrameRepr`, now production)**: decision
  0007 names this explicitly as one of the two things this project builds rather than reuses
  ("The storage abstraction and the state representation, because no existing library models
  Matrix state the way the protocol needs"). Conduit's and Palpo's state-frame designs were read
  for the general approach (not copied -- both are licensed permissively enough to adapt, and
  `crates/hs-state/src/frames.rs`'s module doc already credits the model) rather than reused as
  dependencies, because neither ships as a standalone, embeddable library; both are whole
  homeservers. Building a thin frame representation over `hs-kv` (this project's own storage
  abstraction, itself built for the same "nothing existing models this" reason) was the bake-off's
  conclusion after measuring two real alternatives, not a first-instinct build.
- **The chain-cover auth index** (`crate::chain_cover`): no dependency exists for this; it is
  `PLAN.md` section 6.4's own design for making state resolution v2's auth-difference computation
  fast, directly tied to this project's specific storage layer. Not evaluated against Synapse's
  own auth-chain approach as a library because Synapse is AGPL-3.0 (behavioral reference only, per
  decision 0007's own licensing test) and has no standalone extractable form regardless.
- **This session specifically added no new dependency** (see "Shared dependencies added" below --
  unchanged from session 2): closing the RFC's gap and promoting candidate B were both internal
  reorganization plus one new trait method, not new functionality that could have reused an
  external library.

## Shared dependencies added

Noted in `[workspace.dependencies]` in the root `Cargo.toml` (added by this track):
- `rand_core = "0.6"`: the version `ed25519-dalek` 2.x's optional `rand_core` feature pins (needed
  for `SigningKeyPair::generate` in `hs-model::signing`). Unrelated to and does not replace `rand`
  (0.9), which stays for everything else.

Crate-local additions (already-shared versions, `workspace = true`): `hs-model` added
`ed25519-dalek` (with the `rand_core` feature) and `rand_core`; `hs-state` added `ed25519-dalek`,
`base64`, `sha1`, `proptest` (dev), and, for the bake-off, `hs-kv` and `hs-tables` (both path
dependencies on already-existing, already-stable track-01 crates -- no new workspace dependency
needed) plus `bytes`, `rand` and `tempfile` promoted from dev-only to main dependencies (`rand` and
`tempfile` are used by the bake-off binary and its Fjall-backed tests, which are not `#[cfg(test)]`
paths, so a dev-only dependency would not have been visible to them). `hs-state/Cargo.toml` also
gained a `[[bin]] name = "bakeoff"` target.

## Verify

```
cargo fmt --check -p hs-model -p hs-state
cargo clippy -p hs-model -p hs-state --all-targets -- -D warnings
cargo test -p hs-model -p hs-state
```

All green as of this update (also re-verified against `cargo check -p hs-room` and
`cargo check --workspace`, since this session changed the frozen `StateStore` trait -- see
"Session 3" above): 50 tests in `hs-model`; 59 unit tests in `hs-state` (module layout changed --
`bakeoff::frames`/`bakeoff::generic_store`/`bakeoff::repr`/`bakeoff::varint` moved to top-level
`frames`/`kv_store`/`repr`/`varint`, counts unchanged since nothing was added or removed, only
moved and one method added to two existing trait impls) plus a 512-case property test
(`ruma_cross_check`) plus the `state_res::cross_check_tests` oracle-vs-`ruma-state-res` property
test.

To reproduce the bake-off itself (roughly 10 minutes on the shared development host; the results
already checked in at `crates/hs-state/corpus/results/bakeoff-results.jsonl` do not need
regenerating unless the corpus or a candidate changes):

```
cargo build -p hs-state --release --bin bakeoff
crates/hs-state/corpus/run_bakeoff.sh
```
