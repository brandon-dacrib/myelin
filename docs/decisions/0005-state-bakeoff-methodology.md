# 0005. State-representation bake-off: scoring weights and measurement methodology

Status: accepted, 2026-09-18. Owner: track 02 (state and model).
Consumers: tracks 04 (room actor) and 06 (federation), who build against
whichever candidate wins; the integration lead, for the Phase 0 gate this
bake-off is (`PLAN.md` section 6.3, "WS2 State" in the phase table).

This document is published **before** the bake-off in
`docs/decisions/0006-state-bakeoff-results.md` is run. That ordering is the
point: the weights below are fixed first, and the results document is
required to report the weighted score they produce, not a score
back-fitted to whichever candidate looked best. If a future re-run wants
different weights, it must edit this file first, with its own dated
changelog entry, and only then re-run.

## The three candidates

`PLAN.md` section 6.3 names three, unchanged here:

- **A. Snapshot plus delta chains** (Synapse's model): a group per state
  change, storing a delta from its parent; full snapshots every N hops.
- **B. Deduplicated frames with layered diffs** (Conduit's and Palpo's
  model): state as a sorted set of `(state_key_id, event_sn)` pairs,
  delta-varint compressed; frames deduplicated by content hash of
  `(parent, appended, disposed)`; periodic re-basing to bound layer depth.
- **C. Content-addressed persistent map**: a 32-way trie over
  `state_key_id`'s bits, nodes stored and deduplicated by content hash,
  single-entry subtrees inlined as leaves; a state is its root hash.

All three are implemented behind the frozen `hs_state::api::StateStore`
trait (`crates/hs-state/src/api.rs`, unchanged) over `hs_kv::KvBackend`,
so the same corpus and harness drive all three, and both `hs_kv` backends
available today (`hs_kv::memory::MemoryBackend`,
`hs_kv::fjall_backend::FjallBackend`).

## Scoring weights

Each measurement below is scored per candidate as a ratio to the best
candidate on that measurement (best = 1.0, worse candidates < 1.0 in
proportion, e.g. `best_bytes / this_bytes` for a "smaller is better"
metric), then combined as a weighted sum. Weights sum to 1.0.

| # | Measurement | Weight | Why this weight |
|---|---|---|---|
| 1 | Bytes on disk per state event | 0.15 | Directly determines hosting cost at scale; `PLAN.md`'s stated motivation ("state-group blowup") is fundamentally a size problem. |
| 2 | State-at-event lookup, cold | 0.10 | The first read after a process restart or for a room not recently touched; dominates perceived latency for `/state` and `/sync` after any idle period. |
| 3 | State-at-event lookup, warm | 0.10 | The steady-state room-actor access pattern (track 04): every event needs its prior state to authorize the next one. |
| 4 | Diff cost, states 1 event apart | 0.10 | The dominant real case: almost every state lookup a room actor performs is against its immediate predecessor. |
| 5 | Diff cost, states 100 events apart | 0.05 | Backfill and moderate catch-up sync. |
| 6 | Diff cost, states 10,000 events apart | 0.10 | Deep backfill, a lagging federation peer catching up, and worst-case `/state_ids` for MSC4242. This is exactly where candidate A's "every lookup chases the chain" failure mode and candidate B's "layer depth must be bounded" failure mode are supposed to show up, so it is weighted higher than the 100-apart case rather than folded into it. |
| 7 | Resolution time on forks | 0.15 | State resolution v2/v2.1 is the most CPU-expensive thing this crate does per event received; a representation that makes assembling the conflicted state maps cheap or expensive directly multiplies that cost. Tied with disk bytes as the highest-weighted item because it is the measurement most directly on the hot path of accepting a federation event. |
| 8 | Resident memory (room-owner cache) | 0.10 | `PLAN.md` section 6.3's stated architectural goal for candidate C ("the room owner's cache is literally the store's nodes"); this is where that claim is either confirmed or falsified. |
| 9 | Write amplification | 0.10 | Bytes actually written to the backend per state event ingested, vs. the size of the logical change; determines flash wear and replication/WAL cost on real deployments, not visible from on-disk size alone (a structure can be compact at rest yet write far more than it keeps, notably candidate A's periodic full snapshots). |
| 10 | Compaction / GC cost | 0.05 | Real but a background, schedulable cost, not on any request's critical path; lowest weight for that reason, not because it is unimportant (candidate C's garbage collection is `PLAN.md`'s explicitly named "known failure mode"). |

0.15 + 0.10 + 0.10 + 0.10 + 0.05 + 0.10 + 0.15 + 0.10 + 0.10 + 0.05 = **1.00**.

MSC4242 behavior (explicit-DAG state) is exercised by the fork/backfill
corpus scenarios (below) but is not a separate scored line: every
measurement above already runs against those scenarios, so MSC4242's
effect shows up inside measurements 2-7, not as an eleventh weight.

## Exit criterion

Per `PLAN.md` section 6.3, verbatim: choose C if it is within 20% of the
best candidate on size (#1) and lookup (#2, #3) and wins on diff (#4-#6);
otherwise choose whichever candidate wins the weighted score above. If no
candidate wins the weighted score by more than 5 percentage points over
the runner-up, the results document must say so explicitly rather than
declaring a winner by a coin-flip margin — see its "Decision" section for
which outcome actually happened.

## Measurement method

- **Bytes on disk per state event**: total bytes written to the
  representation's own `hs_kv` keyspace(s) after ingesting a corpus
  scenario, divided by the number of state events ingested. Measured by
  summing `range(..)` value and key lengths over every keyspace the
  candidate owns, on the Fjall backend (the in-memory backend has no
  on-disk representation to measure; its equivalent number is reported as
  resident heap bytes, folded into measurement #8, not #1).
- **Lookup cold**: open a fresh backend handle (Fjall: reopen the database
  file; in-memory: not meaningful, skipped for this specific line — cold
  vs. warm is a page-cache/process-restart distinction that only applies
  to a real disk backend) and time a single `get` for a state key chosen
  uniformly at random from the target state, before any other read
  touches that candidate's keyspaces. Reported as a single measurement
  (n=1 is inherent to "cold"; the harness repeats the whole
  open-then-one-read cycle across many random keys and reports p50/p99
  across those independent cold starts, not across repeated reads of one
  open handle).
- **Lookup warm**: same query repeated 1,000 times against an
  already-open handle after a warm-up pass; p50 and p99 reported.
- **Diff N apart**: pick a state and walk forward N applied state changes
  along the corpus's own history to a second state, then time
  `StateStore::diff` between them. Repeated 100 times at each of N ∈ {1,
  100, 10,000} (10,000 only where the corpus scenario has that much
  linear history; the churn-heavy and policy-room scenarios do, the
  thousand-small-rooms scenario does not and is skipped for that N).
- **Resolution time on forks**: the fork/backfill scenario's `resolve()`
  calls, timed end to end (assembling the state maps in this candidate's
  representation, plus the state-resolution algorithm itself, which is
  identical code across all three candidates — see "What is and is not
  varied" below).
- **Resident memory**: `jemalloc`/allocator-reported RSS delta (via
  `/usr/bin/time -l` on macOS, `max_rss` field) across the whole corpus
  ingestion for that candidate, process isolated per candidate run (not
  measured by running all three in one process, which would conflate
  their allocations).
- **Write amplification**: total bytes written to the KV backend (summed
  from every `put`/`delete` call's key+value length actually issued, not
  the resulting on-disk size which LZ4 and structural dedup shrink)
  divided by the total bytes of the logical `StateDiff`s applied.
- **Compaction/GC cost**: for A, the cost of a full-snapshot write
  (already counted inside write amplification, reported separately here
  as its own line so it is visible); for B, the cost of a re-basing
  event; for C, a full mark-and-sweep pass over the room's reachable node
  set (the simplest correct GC, per `PLAN.md`'s own suggestion) timed
  once per corpus scenario, not amortized.

## What is and is not varied

The state-resolution algorithm itself (`hs_state::state_res::v1`,
`::v2`), the auth-chain / chain-cover index, and the event body store are
**not** part of what this bake-off compares — they are identical code
shared by all three candidates' `StateStore` implementations, exactly as
`crate::store::InMemoryStateStore` already demonstrates is possible.  Only
the representation of a resolved state map (`state_at`, `get`, `diff`,
`apply`) differs between candidates. This keeps the bake-off honest about
what it is actually measuring: state *storage*, not state *resolution*,
per `PLAN.md` section 6.3's own framing ("state storage must be
redesigned... a faster language does not fix" a storage problem).

## Known, declared limitations of this run

Recorded here in advance so the results document cannot understate them
after the fact:

- **No real-room corpus.** `PLAN.md` section 6.3's corpus item 1 (a public
  room's full state history joined from a throwaway server) and item 2
  (a real high-churn support room) require network access and a running
  homeserver to join from; neither is available in this environment.
  Every corpus scenario is synthetic (see
  `crates/hs-state/corpus/generators.rs`), generated to match the
  *statistical shape* PLAN.md describes (membership churn rate, policy
  room size, fork/backfill topology) rather than collected from a real
  room. This is a real limitation, not a formality: synthetic churn is
  more uniform than real churn (which clusters in bursts around raids,
  moderation actions and client bugs), so absolute numbers from this run
  should be treated as directionally indicative, not load-bearing for
  capacity planning.
- **Single machine, shared with five other agents.** The shared 10-core,
  16 GB host is running concurrent, unrelated cargo builds from other
  tracks throughout this run. Wall-clock latency numbers therefore have
  more noise than a dedicated benchmarking host would produce; the
  harness reports p50/p99 specifically to make that noise visible rather
  than hiding it behind a mean, but absolute latency numbers should not
  be read as production capacity figures.
- **Corpus sizes are scaled down from `PLAN.md`'s description** (100k
  members / years of churn, hundreds of thousands of membership changes,
  100k policy events) to sizes that complete in minutes rather than hours
  on shared hardware; the exact scale-down factor for each scenario is
  recorded next to its generator and repeated in the results document.
  The measurements that are ratios or per-event rates (bytes per state
  event, write amplification) are expected to hold at full scale; the
  measurements that are about absolute structure depth (diff at 10,000
  events apart, resolution on forks) are the ones scaling down most
  affects, and the results document says so again at the point those
  numbers are reported.
- **One backend family (Fjall) has a working implementation here**;
  PostgreSQL and SlateDB (`PLAN.md` section 6.5) do not exist yet as
  `hs_kv::KvBackend` implementations, so this run cannot say how the
  ranking changes on a backend with network round-trips and a different
  compaction story. The in-memory backend is included for latency-floor
  and correctness comparison, not as a production candidate.
