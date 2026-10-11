# RFC 0024: a cap on Fjall's total write buffer

- Status: accepted; implemented 2026-10-10 by track 01 (`agent/fjall-write-buffer`), with one
  deviation from "What is asked", in "Outcome" at the end
- Date: 2026-10-10
- Owner of the change: track 01 (`crates/hs-kv/src/fjall_backend.rs`)
- Asked by: track 06, from the 2026-10-10 leak hunt (`docs/status/06-federation.md`)

## The problem

The demo homeserver's resident memory crept at a constant 13 MiB an hour while idle, on every
build of 2026-10-10, until the pod was over its 2 GiB and then 3 GiB limit together with the room
cache after a large join. The federation sender's share of that was found and bounded on the
federation side (below), but the mechanism it exposed is in the storage layer and applies to every
periodic writer in the process, not only to the sender.

Fjall keeps every write in the memtable of its keyspace until that memtable reaches
`max_memtable_size` (64 MiB by default in Fjall 3.1), and only then flushes it to a table on disk.
A key written again does not replace its previous version in the memtable; each version is a new
entry until the flush. `hs-kv` opens one keyspace per table (139 on the demo at boot) with
`KeyspaceCreateOptions::default()`, and the database-wide `max_write_buffer_size` is `None`, so
nothing bounds the sum: a slow writer's memtable grows for hours at the writer's rate, and a
hundred keyspaces can each hold up to 64 MiB of versions before anything is flushed.

Measured in `crates/hs-federation/tests/sender_soak.rs` (ignored; see its module docs): the same
sender code, the same 25 retry attempts a second against 50 unresolvable destinations, each attempt
writing one small retry-state row:

| backend for the two stores | RSS after a 2-minute warm-up | RSS at 15 minutes | slope     |
|----------------------------|------------------------------|-------------------|-----------|
| `FjallBackend` (tempdir)   | 35,824 KiB                   | 37,600 KiB        | +8.2 MiB/h |
| `MemoryBackend`            | 27,776 KiB                   | 25,728 KiB        | flat      |

The federation sender now writes that row once per backoff step instead of per attempt (a dead
destination settles at one write an hour), and the client's destination store backs off a
destination that does not resolve, so the sender is no longer a fast writer. Presence, device-list
positions, cluster leases, appservice cursors and anything else that rewrites a key on a timer
still feed their memtables the same way.

## What is asked

In `FjallBackend::open`, set a database-wide write-buffer cap, so that when the memtables together
pass it Fjall flushes the largest, whatever its own size:

```rust
fjall::OptimisticTxDatabase::builder(path)
    .max_write_buffer_size(Some(128 * 1024 * 1024))
```

(`fjall::Builder::max_write_buffer_size(Option<u64>)`, Fjall 3.1; `None` is the default). A
value of 64-128 MiB bounds the memtables' total at the cost of more frequent, smaller flushes
for the busiest keyspace; the journal is unaffected. A smaller per-keyspace
`max_memtable_size` for the small, frequently rewritten tables (destination state, positions,
leases) is the finer alternative and can come later.

Make the cap an `hs-kv` constant first; a `storage.embedded.write_buffer_bytes` setting can follow
if an operator ever needs to tune it.

## How to verify

- `SOAK_MINUTES=15 cargo test -p hs-federation --test sender_soak -- --ignored --nocapture`
  with the Fjall backend (the default) is as flat as `SOAK_BACKEND=memory`, once the cap is low
  enough to be reached in the run (`SOAK_MAX_BACKOFF_SECS=2` keeps the attempt rate at 25/s).
- `process_resident_memory_bytes` on `/metrics` (added by track 06 in `hs-cli`) stops creeping
  linearly on the demo while idle, or saw-tooths under the cap.

## Why not in track 06

`crates/hs-kv` is track 01's. The federation side has done what it can: fewer writes. The cap is
one line in the backend's constructor and belongs with the backend's other defaults.

## Outcome (track 01, 2026-10-10)

Implemented in `crates/hs-kv/src/fjall_backend.rs` (module docs, "The write buffer is capped"),
with one deviation: **`fjall::Builder::max_write_buffer_size` is not used, because in Fjall 3.1.10
it does nothing.** The builder stores the value in `db_config.max_write_buffer_size_in_bytes`,
the method is `#[deprecated = "todo"]` and `#[doc(hidden)]`, and nothing reads the field (the
only flush triggers are a keyspace's own `max_memtable_size`, checked on insert, and journal
eviction at `max_journaling_size`, 512 MiB). So the backend enforces the cap itself:

- **A database-wide cap, enforced after every commit**: `FjallBackend` reads Fjall's
  `write_buffer_size()` (one atomic load) and, when it is over the cap, rotates the memtable of
  every Fjall keyspace that has no sealed memtable awaiting a flush (`Keyspace::rotate_memtable`,
  `sealed_memtable_count`). A keyspace already being flushed is skipped, so a crossing is one
  flush round, not a storm. The cap is `FJALL_WRITE_BUFFER_CAP` = **32 MiB**, not the 128 MiB
  suggested: on the shared layout (decision 0024) the database has one memtable, so the number
  that bounds an idle server's creep is the memtable size, and the cap exists for directories in
  the per-table layout (the demo's, if its data directory predates 2026-10-01), where it bounds
  the sum of a hundred-odd memtables.
- **A smaller `max_memtable_size` for every Fjall keyspace the backend creates**:
  `FJALL_MEMTABLE_SIZE` = **16 MiB** (Fjall's default is 64; it recommends 8 to 64). Fjall persists
  the size at creation, so a keyspace recovered from an existing directory keeps its 64 MiB and is
  covered by the cap. Per-table sizing is not possible: every table is a prefix in one keyspace.
- Both are `FjallOptions` on `FjallBackend::open_with_options`; `open` uses the defaults. No
  `hs-config` setting yet (a constant first, as asked); wiring one is a field in
  `EmbeddedStorageConfig` and one line in `hs-cli`'s bootstrap.
- Observability: `hs_kv_fjall_write_buffer_bytes`, `hs_kv_fjall_write_buffer_cap_bytes`,
  `hs_kv_fjall_write_buffer_rotations_total` and `hs_kv_fjall_sealed_memtables`, read live at
  scrape from every open backend by a collector that `hs_kv::metrics::register_metrics` registers
  (already called by `hs serve`); an `info` line at open names the cap and memtable size.
- Proof: `crates/hs-kv/tests/fjall_write_buffer.rs` (the cap rotates and flushes, reads stay
  right across the flushes and a reopen; a small memtable size makes Fjall flush on its own) and
  `tests/fjall_write_buffer_soak.rs` (ignored, five minutes, RSS from `ps`), whose numbers are in
  `docs/status/01-storage-engine.md`, "The Fjall write buffer is capped (2026-10-10)".
