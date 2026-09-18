# 02. State and model

Wave 1, starts day one. Owns the decision the whole plan hinges on.

**Expert profile.** Matrix protocol depth (all twelve room versions, event auth, state resolution v1, v2 and v2.1, redaction algorithms, canonical JSON), algorithms and persistent data structures, Ruma-contributor familiarity.

**Mission.** Build the event model and the state engine, run the state-representation bake-off on real room histories, and freeze the `hs-state` API that the room actor and federation build on. See `PLAN.md` sections 3, 4 (D3, D4) and 6.

**Owns.** `hs-model` (events over Ruma with cached canonical bytes and hashes, internal metadata flags, room-version capability table, redaction, signing helpers), `hs-state` (event auth per version, state resolution, the state representation, chain-cover index, MSC4242 tables), the bake-off corpus and harness, the state-resolution oracle used in tests.

**Provides.** Week 2: `hs-model` types and the room-version table (versions 1 to 12 plus the unstable ones Synapse ships). Week 6: `hs-state` API (`state_at(event_sn)`, `diff(a, b)`, `apply(root, changes)`, `resolve(forks)`, chain-cover queries). Week 12: the representation decision and report.

**Consumes.** `hs-kv` and `hs-tables` from 01 (uses the in-memory backend until week 4).

**Day-one work.**
- `hs-model`: identifiers, event wrapper, canonical JSON and hashing tests derived from the spec appendices and from Synapse's documented quirks (re-expressed, not copied), redaction algorithm per version with vectors, room-version capability table.
- Event auth for versions 1 to 12 written from the spec text, cross-checked against `ruma-state-res`'s auth implementation on random events.
- State resolution v1 in-house; v2 and v2.1 through `ruma-state-res`; an independent oracle implementation of v2 and v2.1 straight from the spec, used only in tests.
- Start collecting the corpus: a throwaway homeserver joins a very large public room and a high-churn support room; scripts dump their events; synthetic generators for the moderation-policy room and for fork and backfill scenarios.

**Phase 0 deliverables.** The three representation prototypes from section 6.3 (snapshot plus delta chains; deduplicated frames with layered diffs; content-addressed persistent map) behind the same API; the benchmark harness measuring size, lookup, diff, resolution, memory, write amplification and GC on every corpus and on both backends from 01; weights published before the run; the decision report; the chain-cover index in interned form with tests; MSC4242 edge tables designed so `prev_state_events` can be stored from the first federation milestone.

**Phase 1 and 2 deliverables.** Chain-cover rebuild and background verification; state garbage collection; partial-state (faster join) support in the API; `state_after` helpers for 05; MSC4242 receiving and serving with 06; performance work against the corpus benchmarks.

**Definition of done.** Cross-implementation property tests (ours, Ruma, the oracle, and Synapse's own implementation driven through the Python harness that 14 provides) agree on 10k random DAGs per room version; spec test vectors pass; fuzz targets for event JSON and auth run continuously; the bake-off report exists and CI tracks the corpus benchmarks.

**References.** Spec room version pages v1 to v12 and the appendices; `refs/ruma/crates/ruma-state-res`; `refs/synapse/synapse/state/v1.py` and `v2.py`, `refs/synapse/synapse/event_auth.py` and `refs/synapse/docs/auth_chain_difference_algorithm.md` (read for behavior and the chain-cover idea only, AGPL); `refs/palpo/crates/core/src/state/` and the `room_state_frames` tables in `refs/palpo/crates/data/migrations/2025-06-05-063543_main/up.sql`; `refs/conduit/conduit/src/service/rooms/state_compressor/`; MSC4297, MSC4242, the Project Hydra post.

**Open questions to settle first.** Branching factor and node hash for the persistent map; GC strategy (per-room refcounts versus mark and sweep); representation of rejected and soft-failed events and of unknown state in partial-state rooms; ownership boundary of interning between 01 and 02.

**Risks.** Corpus collection etiquette (use a throwaway server, respect rate limits, public rooms only); a bake-off with no clear winner (the weights are fixed in advance to prevent drift).
