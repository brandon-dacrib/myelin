# 01. Storage engine

Wave 1, starts day one. Owner of the most-consumed interfaces in the project.

**Expert profile.** Storage-engine internals (LSM and B-tree, MVCC, isolation levels), PostgreSQL internals and tuning, order-preserving key encodings, Rust async and unsafe-free performance work.

**Mission.** Provide one ordered, transactional key-value abstraction with three production backends and one test backend, plus a typed table layer with declarative secondary indexes, so every other track stores data without hand-writing index maintenance and without knowing which backend is underneath. See `PLAN.md` sections 4 (D1), 6.5, 6.7 and Appendix A.

**Owns.** `hs-kv`, `hs-tables`, `hs-search` (`tantivy` per shard), the backend conformance suite, the store-level export and import tooling, the index verifier.

**Provides.** Week 2: `hs-kv` trait v0 (snapshot reads, serializable transactions with retry, range scans, multi-get, atomic add, watches) and the in-memory backend. Week 4: `hs-tables` (tuple-style order-preserving key encoding, keyspaces, unique and non-unique composite and partial indexes maintained inside the transaction, migrations, interning). Week 12: Fjall and PostgreSQL backends passing the conformance suite.

**Consumes.** `hs-model` identifier types from 02 for typed keys. Nothing else.

**Day-one work.**
- Write the trait and its semantic contract as a document first: what "serializable" guarantees on each backend, transaction size limits, watch semantics (hints, not guarantees), key and value size limits, ordering rules.
- In-memory backend and the conformance suite: write skew, lost update, phantom reads, snapshot visibility, range scan boundaries, concurrent watches, crash recovery (kill -9 during a write batch, reopen, verify invariants).
- Fjall 3 backend with keyspaces per table, per-level compression, key-value separation for large values.

**Phase 0 deliverables (weeks 1 to 12).**
- Trait v0 frozen (week 2), tables API frozen (week 4), interning tables (`room_sn`, `user_sn`, `server_sn`, `event_sn`, `state_key_id`, `type_id`) with reverse lookups and caches.
- PostgreSQL backend: one table per keyspace `(k bytea primary key, v bytea)`, `SERIALIZABLE` with retry, pipelined multi-get via `= ANY($1)`, range scans on the primary key, `LISTEN` and `NOTIFY` watches, statement caching, connection pool; documented `pgbouncer` constraints (session mode only for watch connections).
- Zstd dictionary evaluation for event JSON with 02, decided by size and latency numbers.
- Micro-benchmarks for every operation on both architectures; conformance suite green on in-memory, Fjall and PostgreSQL.
- A written cost model per backend that 04, 05 and 06 use to design access patterns.

**Phase 1 and 2 deliverables.** SlateDB backend behind a feature flag with per-shard databases and fencing hooks for 03; index verifier and repair job; `hs store export` and `import` for single-node to cluster moves; per-backend metrics; `tantivy` search index per shard with rebuild from the store; backup and restore guidance per backend.

**Definition of done.** Conformance suite including crash tests green on all backends in CI on amd64 and arm64; property tests for index maintenance; benchmarks tracked; the trait contract document is the reference other tracks cite.

**References.** FoundationDB's data modeling documentation (tuple layer, subspaces, directories); Fjall 3 docs and the 3.0 release post; `refs/conduit/conduit/src/database/abstraction/` (the `Tree` abstraction and both backends); `refs/palpo/crates/data/src/schema.rs` and `refs/palpo/crates/data/migrations/`; SlateDB docs on manifest fencing; PostgreSQL docs on serializable snapshot isolation.

**Open questions to settle first.** Varint versus fixed-width integers inside keys (range scans need order preservation); whether PostgreSQL uses one table per keyspace (recommended for vacuum locality) or one table with a prefix; maximum transaction size and how 04 batches persistence under it; blob separation thresholds.

**Risks.** Fjall's single-writer optimistic transactions and PostgreSQL's SSI are not identical; the conformance suite must pin the semantics applications may rely on, and the trait doc must forbid long-running transactions.
