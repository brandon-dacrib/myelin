# 0006. State-representation bake-off: results and decision

Status: accepted, 2026-09-18. Owner: track 02 (state and model).
Consumers: tracks 04 (room actor) and 06 (federation) — see
`docs/status/02-state-and-model.md` section "Implications for tracks 04 and
06" for what this decision means for their access patterns; the
integration lead, as the Phase 0 "WS2 State" gate.

Weights and methodology were published in
`docs/decisions/0005-state-bakeoff-methodology.md` **before** this run. This
document reports what actually happened, unedited by the weights (the
weights were not touched after seeing these numbers).

## What ran

All three candidates (`crates/hs-state/src/bakeoff/{snapshot_delta,frames,persistent_map}.rs`,
labeled A, B, C below) against both `hs_kv` backends
(`hs_kv::memory::MemoryBackend`, `hs_kv::fjall_backend::FjallBackend`), against
five corpus scenarios (`crates/hs-state/corpus/generators.rs`), on the
project's shared development host (10 cores, 16 GB, five other agents
building concurrently in the same repository at the time). Raw output:
`crates/hs-state/corpus/results/bakeoff-results.jsonl` (30 lines, one JSON
object per candidate/backend/scenario combination, produced by
`crates/hs-state/src/bin/bakeoff.rs` via
`crates/hs-state/corpus/run_bakeoff.sh`). Every number below is read
directly from that file; none is estimated or interpolated.

Actual corpus sizes generated (all smaller than `PLAN.md`'s named scale,
per the methodology document's declared scale-down):

| Scenario | Events | State events |
|---|---|---|
| `large_room_membership_churn` | 2,250 | 2,250 |
| `high_churn_support_room` | 24,003 | 24,003 |
| `moderation_policy_room` | 8,003 | 8,003 |
| `fork_and_backfill` | 1,834 | 609 |
| `small_rooms` (200 rooms) | 2,400 | 1,800 |

## Headline numbers (Fjall backend, pooled across all five scenarios)

Fjall is the backend these numbers are scored on: it is the only
production-shaped backend of the two implemented so far (`PLAN.md` section
6.5; PostgreSQL and SlateDB do not exist yet). MemoryBackend results are
reported separately below and were **not** used in the weighted score — see
"Why MemoryBackend is excluded from the score."

| Measurement | A (snapshot+delta) | B (frames) | C (persistent map) | Best |
|---|---|---|---|---|
| Bytes on disk / state event | 302.3 | **105.4** | 771.1 | B |
| Lookup, cold, p50 (µs, avg across scenarios) | **11.1** | 22.3 | 16.6 | A |
| Lookup, warm, p50 (µs, avg across scenarios) | 3.5 | 13.1 | **2.4** | C |
| Diff, 1 apart (µs, avg across scenarios) | **0.33** | 0.34 | 4.01 | A |
| Diff, 100 apart (µs, avg across scenarios with that probe) | 26.5 | 41.7 | **16.1** | C |
| Diff, ~8,000-10,000 apart (µs, avg across scenarios with that probe) | **197.9** | 224.9 | 742.3 | A |
| Resolution time on forks, p50 (µs, `fork_and_backfill`, n=25) | **1435.7** | 1475.5 | 1620.5 | A |
| Resident memory, avg RSS across scenarios (MB) | 39.5 | **31.2** | 57.2 | B |
| Write amplification (bytes written / logical bytes changed) | 25.2x | **8.8x** | 64.3x | B |
| Compaction/GC overhead (extra time beyond ingest, as a fraction of ingest time) | 0% (inline) | 0% (inline) | 21.0% (separate `gc()` pass) | A/B tie |

Bold = best candidate on that row. Full per-scenario numbers (not just the
pooled average) are in "Per-scenario detail" below; the JSONL has
everything, including p99s and every scenario's own diff-probe labels.

## Weighted score

Per measurement, `score = best_value / this_value` (lower-is-better rows;
every row above is lower-is-better) except "compaction/GC overhead," which
can legitimately be `0`, so it is scored as `1 - min(1, overhead_fraction)`
instead (documented deviation from the ratio formula, noted here because
the ratio formula is undefined when the best value is zero).

| # | Measurement | Weight | A score | B score | C score |
|---|---|---|---|---|---|
| 1 | Bytes on disk / state event | 0.15 | 0.349 | 1.000 | 0.137 |
| 2 | Lookup cold | 0.10 | 1.000 | 0.500 | 0.671 |
| 3 | Lookup warm | 0.10 | 0.693 | 0.185 | 1.000 |
| 4 | Diff 1 apart | 0.10 | 1.000 | 0.974 | 0.083 |
| 5 | Diff 100 apart | 0.05 | 0.610 | 0.387 | 1.000 |
| 6 | Diff ~10,000 apart | 0.10 | 1.000 | 0.880 | 0.267 |
| 7 | Resolution time on forks | 0.15 | 1.000 | 0.973 | 0.886 |
| 8 | Resident memory | 0.10 | 0.791 | 1.000 | 0.546 |
| 9 | Write amplification | 0.10 | 0.349 | 1.000 | 0.137 |
| 10 | Compaction/GC overhead | 0.05 | 1.000 | 1.000 | 0.790 |
| | **Weighted total** | 1.00 | **0.766** | **0.819** | **0.513** |

## Exit criterion

`PLAN.md` section 6.3, verbatim: "choose C if it is within 20 percent of
the best candidate on size and lookup and wins on diff; otherwise choose
the candidate that wins on the weighted score."

C's size (771.1 bytes/state event) is **630% of B's** (105.4), nowhere
near the 20% band. **C fails the exit criterion's size test outright**, so
the decision falls through to the weighted score without needing to check
the lookup or diff conditions.

## Decision

**B (deduplicated frames with layered diffs) wins the weighted score**,
0.819 against A's 0.766 — a margin of **5.3 percentage points**.

This clears the methodology document's 5-point bar for "decisive" by a
hair, and this document says so plainly rather than rounding it up to a
comfortable win: **B is the mechanical winner, but the margin is thin, and
A is extremely competitive.** A wins outright on four of the ten scored
rows, including cold lookup, both of the two diff distances weighted at
0.10, and resolution time on forks (the single highest-weighted row,
tied with disk bytes at 0.15). B's win is driven almost entirely by two
correlated rows — disk bytes (0.15) and write amplification (0.10), both
of which reward the same property (compact, varint-delta-compressed
frames) — plus resident memory. If either of those two rows had been
weighted 3 points lower, A would have won instead. A reader who weights
"predictable low-latency lookup and diff on the hot path" over "small
footprint at rest" would reasonably read this same data and pick A.

**Recommendation:** implement the room actor and federation's state
storage (tracks 04 and 06) against candidate B, since it is the published
rule's actual winner and it is also the *simpler* of the two remaining
candidates (a linked list of content-addressed frames, vs. A's group
table needing separate snapshot-interval tuning) — but do not treat this
as a closed question. The margin is inside the range a real-room corpus
(this run's biggest declared limitation) could plausibly flip, and this
document recommends re-running the bake-off against real-room data before
treating candidate B as final for the production PostgreSQL/SlateDB
backends once those exist (`PLAN.md` section 6.5) — see "What would change
this decision" below.

C, this bake-off's stated prior, **lost decisively** here, and the biggest
single reason is implementation-specific, not structural: see "Why C lost"
below before drawing conclusions about content-addressed persistent maps
in general.

## Why C lost

C's core structural claim — "diff between any two states skips identical
subtrees" — is real and visible in this data: on `fork_and_backfill`'s
deep-distance probe (comparing the state right after initial joins to the
state after 25 rounds of forking and merging, most of which touched a
single contested key), C's diff cost is **0.041 µs**, versus 26.4 µs (A)
and 26.5 µs (B) — effectively free, because almost every subtree hash
matches and the comparison never descends into it. This is exactly the
operation `PLAN.md` section 6.3 says matters most for state resolution and
`/sync` deltas, and it is the one place in this data where C's structural
advantage shows up completely undiluted by implementation overhead.

Everywhere else, C loses, and the reason is visible in the implementation,
not just the numbers: **this candidate's 32-way trie is 6-7 levels deep
for a `StateKeyId` (a `u32`), so one single-key state change touches up to
seven trie nodes, each an independent content-addressed KV record**
(`crates/hs-state/src/bakeoff/persistent_map.rs`, `digit()`'s level
count). Candidates A and B write **exactly one record per state change**,
always. That structural difference, not a flaw in content-addressed
persistent maps as an idea, is the direct cause of:

- **7x-ish more distinct KV records for the same logical history**, which
  is most of why C's bytes-on-disk and write-amplification numbers are
  worse than A's and much worse than B's (worse than a naive per-key
  estimate would suggest, because Fjall's per-record framing overhead is
  paid seven times over per state change, not amortized across the seven
  nodes the way a single wider record would).
- **A GC pass that is not optional**: A and B's periodic
  snapshot/rebase is folded into ordinary `apply` calls and never
  leaves orphaned data; C's path-compression on delete
  (`PersistentMapRepr::delete_rec`'s "path compression" comment)
  deliberately leaves the pre-compression node unreachable, so a
  real deployment would need to run `PersistentMapRepr::gc` on a
  schedule — measured here at a real, non-trivial 21% of ingest time
  pooled across scenarios (and 96.7 ms / 234.9 ms in absolute terms on the
  policy and support-churn scenarios respectively), reclaiming megabytes
  each time.

A wider fan-out (a 256-way trie, one byte per digit, four levels instead
of seven for a 32-bit key) or a coarser sharing granularity (batching
several `StateKeyId` bits per node, trading some structural-sharing
precision for fewer, larger records) would very plausibly change this
result substantially — this was not attempted here for lack of remaining
time in this bake-off, and is the most promising unfinished lead if a
future pass revisits candidate C rather than treating this run as the
final word on content-addressed persistent maps as an approach.

## Why MemoryBackend is excluded from the score

`hs_kv::memory::MemoryBackend` clones the entire keyspace's `BTreeMap` on
every write-transaction commit (`crates/hs-kv/src/memory.rs`,
`commit`'s `(**state.keyspaces...).clone()`) — an O(current keyspace size)
cost per commit, by design and explicitly documented in that crate as "not
a performance backend." Every candidate here issues one commit per
`apply` (or, for a multi-node write like C's, one commit for the whole
batch — see `crates/hs-state/src/bakeoff/persistent_map.rs`'s
`WriteBatch`/`commit_batch`, added specifically to avoid seven commits per
key change), so ingesting `N` state events against MemoryBackend costs
`O(N)` commits at up to `O(N)` each: `O(N²)` in the worst case. This is
visible directly in the data: `high_churn_support_room` (24,003 events)
took **13.1 s** (A), **12.5 s** (B) and **146.5 s** (C) against
MemoryBackend, versus **485 ms**, **304 ms** and **1.3 s** respectively
against Fjall — a 27x-113x slowdown from the reference backend alone, not
from anything about the candidates being compared. Scoring MemoryBackend
numbers into the weighted total would have been scoring `hs_kv`'s
reference implementation, not the three candidates. MemoryBackend's
numbers are recorded in the raw JSONL (`backend: "memory"` rows) for
correctness cross-checking and as a latency floor, per the methodology
document's original intent, not for the decision itself.

## Per-scenario detail

Diff cost at distance, by scenario (µs, p50 of 20 repetitions), Fjall
backend:

| Scenario | Distance | A | B | C |
|---|---|---|---|---|
| `large_room_membership_churn` | 1 apart | 0.42 | 0.46 | 5.67 |
| | 100 apart | 33.2 | 47.9 | 17.7 |
| | ~2,250 apart (full history) | 109.9 | 127.2 | 363.7 |
| `high_churn_support_room` | 1 apart | 0.42 | 0.42 | 6.38 |
| | 100 apart | 30.9 | 37.8 | 17.8 |
| | ~24,000 apart (full history) | 15.4 | 18.9 | 60.4 |
| `moderation_policy_room` | 1 apart | 0.42 | 0.42 | 5.58 |
| | 100 apart | 39.7 | 79.3 | 24.4 |
| | ~7,900 apart (full history) | 639.8 | 727.2 | 2545.0 |
| `fork_and_backfill` | 1 apart | 0.042 | 0.042 | 0.041 |
| | 100 apart | 2.1 | 1.9 | 4.6 |
| | deep backfill stand-in (full history) | 26.4 | 26.5 | **0.041** |
| `small_rooms` (200 rooms) | 1 apart (avg of 200) | 0.38 | 0.39 | 3.0 |

The `moderation_policy_room` full-history row is the starkest illustration
of `PLAN.md`'s "state-group blowup": 8,000 state events, each a distinct
key that is never overwritten, is the worst case for a periodic-snapshot
representation (A must rebuild and store the *entire* 8,000-key map every
50 hops) and it shows: A's full-history diff costs 640 µs, B's 727 µs,
both dominated by materializing a large map on both sides because the two
probed states are not on the same delta/layer chain. C's equivalent number
here is *worse*, not better (2,545 µs) — because, per "Why C lost" above,
comparing two states that share almost nothing (every one of 8,000 keys
differs) means C pays its per-node overhead across the *entire* trie with
no subtree-skip benefit to offset it; C's structural advantage only pays
off when two states are *mostly* the same, which `fork_and_backfill`'s
probe is and `moderation_policy_room`'s full-history probe is not.

Ingest wall time (ms), Fjall backend, for context (not a scored
measurement on its own, but explains why MemoryBackend's slowdown is
visible so clearly on `high_churn_support_room` specifically — it is by
far the largest scenario by event count):

| Scenario | A | B | C |
|---|---|---|---|
| `large_room_membership_churn` (2,250 events) | 47.8 | 37.7 | 69.8 |
| `high_churn_support_room` (24,003 events) | 485.3 | 303.6 | 1317.5 |
| `moderation_policy_room` (8,003 events) | 505.5 | 119.8 | 264.5 |
| `fork_and_backfill` (1,834 events) | 49.8 | 42.4 | 54.0 |
| `small_rooms` (200 rooms, 2,400 events) | 38.2 | 17.7 | 47.1 |

## Content-addressed dedup, observed

Candidate B's frame deduplication (identical `(parent, appended,
disposed)` triple hashes to the same frame, skipping the write) fired 174
times on `fork_and_backfill` and zero times everywhere else in this
corpus. This is a real, honest, unglamorous finding: B's dedup only pays
off when the *exact same delta from the exact same parent* recurs, which
this synthetic corpus produces occasionally (branches racing to set the
same topic value format from the same base) but not often. `PLAN.md`
describes B's dedup as "automatic," which is true, but this run does not
support a claim that it saves much space in ordinary (non-forking)
traffic — the bytes-on-disk win B shows in this data comes from
delta-varint compression of the appended/disposed lists, not from content
hash collisions across unrelated writes. Candidate C's dedup counter
(`dedup_hits` in the raw JSONL) shows the same pattern for the same
reason (identical trie nodes only recur across structurally similar
writes).

## What would change this decision

- **A real-room corpus** (this run's largest declared limitation, see
  `docs/decisions/0005-state-bakeoff-methodology.md`). Real membership
  churn clusters in bursts (raids, moderation waves, client bugs) rather
  than the uniform cycling this corpus generates; a burst pattern changes
  which delta-chain-depth or layer-depth values are actually hit between
  snapshots/rebases, which is precisely the input A's and B's periodic
  compaction cadence (`SNAPSHOT_INTERVAL`/`REBASE_INTERVAL`, both 50 here)
  is sensitive to.
- **A wider-fan-out or coarser-grain revision of candidate C** (see "Why C
  lost"). This run's 32-way, 7-level trie is one specific design point in
  a large space; a design that writes fewer, larger records per key
  change could plausibly close most or all of the 5.3-point gap, since
  C already wins outright on warm lookup, 100-apart diff, and the
  fork-diff case that is the entire point of the "operation state
  resolution... needs" argument `PLAN.md` makes for it.
- **A PostgreSQL or SlateDB backend** (`PLAN.md` section 6.5, not
  implemented yet): network round-trip latency changes the relative cost
  of "one record per write" (A, B) versus "up to seven records per write"
  (C) differently than a local, memory-mapped Fjall database does; this
  could narrow or widen the gap in either direction.

## Verify

```
cargo build -p hs-state --release --bin bakeoff
crates/hs-state/corpus/run_bakeoff.sh crates/hs-state/corpus/results/bakeoff-results.jsonl 200
```

Raw results: `crates/hs-state/corpus/results/bakeoff-results.jsonl` (30
lines). Candidate implementations:
`crates/hs-state/src/bakeoff/{snapshot_delta,frames,persistent_map}.rs`.
Corpus generators: `crates/hs-state/corpus/generators.rs`. Harness:
`crates/hs-state/src/bin/bakeoff.rs`. Driver script:
`crates/hs-state/corpus/run_bakeoff.sh`.
