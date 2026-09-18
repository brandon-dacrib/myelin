# 05. Sync

Wave 2, code starts week 8 on the room protocol; design and the token codec start day one.

**Expert profile.** Sync engines, simplified sliding sync (MSC4186 and its extension MSCs), Element X and `matrix-rust-sdk` behaviors, latency engineering.

**Mission.** The user session actor and both sync APIs, built on per-user feeds instead of a global stream, with failover that preserves tokens. See `PLAN.md` sections 5.4 and 6.6.

**Owns.** `hs-user`: `/sync` v2, `initialSync`, simplified sliding sync with the e2ee, to-device, account data, receipts, typing and thread-subscription extensions, filters and lazy loading, feeds and the hybrid fan-out threshold, sync tokens, per-device sliding sync connection state, presence and typing distribution to sessions, account data and tags, thread subscriptions.

**Provides.** Week 8 (jointly with 04): the user session protocol (subscribe, publish, cursors). The token codec (opaque, versioned, embeds `feed_seq` plus the independent cursors) and the rule for accepting Synapse-issued tokens after import (with 13).

**Consumes.** 04 publish stream, 03 ownership, 07 `Requester`, 08 cursors (device lists, one-time-key counts, to-device), 10 counts, 01 store.

**Day-one work.** Read MSC4186 and the accepted and pending extensions; design the token codec with property tests; design the feed (coalescing, retention relative to the oldest live device token, pruning); design sliding-sync connection persistence; with 14, record Element X sliding-sync request sequences from `matrix-rust-sdk` as a test corpus.

**Phase 0 deliverables.** Design documents; token codec with tests; feed prototype on the KV with fan-out benchmarks that set the hybrid threshold.

**Phase 1 and 2 deliverables.** `/sync` v2 complete (filters, lazy members, `full_state`, `timeout`, presence, to-device, device lists, unread and thread notification counts, `state_after`); sliding sync complete with extensions; the session actor with failover from the feed; response caching; performance to the p99 targets.

**Definition of done.** Complement sync tests green; `matrix-rust-sdk` sliding-sync tests (the Element X path) green; differential sync output against Synapse on scripted workloads clean after normalization; p99 targets met in the nightly load run; chaos: session failover preserves tokens and never skips or duplicates events.

**References.** MSC4186 and the extension MSCs (MSC4538 and siblings); `refs/synapse/synapse/handlers/sliding_sync/` and `sync.py` (behavior, AGPL, read only); `refs/synapse/synapse/types/__init__.py` for the legacy token shape 13 must accept; `refs/palpo/crates/server/src/sync_v5.rs`; `matrix-rust-sdk` sliding sync client code.

**Open questions to settle first.** Per-user session holding per-device connections (recommended) versus per-device sessions; how invites, knocks and leaves enter the feed; feed retention; the hybrid threshold.

**Risks.** `limited`, `prev_batch` and state-at-timeline-start semantics are where clients break; differential testing is not optional here.
