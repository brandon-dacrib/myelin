# 0022: Every `hs-kv` keyspace on Fjall shares one Fjall keyspace (2026-10-01)

Status: accepted (track 01). Closes the `docs/next-steps.md` gap "A first boot over an empty
data directory takes about five seconds".

## Context

`PLAN.md` section 6.5 maps the embedded backend as "one keyspace per table": each `hs-kv`
keyspace was its own Fjall keyspace. `hs serve` opens about eighty of them at boot, and
creating a Fjall keyspace is expensive and serial. Fjall 3.1 does it under its keyspace write
lock: a tree directory and manifest written and fsynced, the keyspace's configuration ingested
into Fjall's meta keyspace as a new table (file, fsync, directory fsync, manifest commit), and
a compaction of the meta keyspace with a hard-coded L0 threshold of two. On this project's
desktop that was 4.5 of a 5.0 to 5.4 s first boot (status 01, 2026-09-27); every later boot
was 0.4 s.

Fjall has no way to create several keyspaces under one sync, and its lock serializes creations,
so neither batching nor parallel creation was available. Opening keyspaces lazily would only
move the cost to each table's first request.

## Decision

- **One Fjall keyspace, `_hs_kv_shared`, holds every `hs-kv` keyspace** behind a prefix of one
  length byte and the keyspace's name: a key `k` of keyspace `n` is stored as
  `[len(n)][n][k]`. The prefix is free of collisions (the length comes first), needs no
  catalog, and costs nothing to "create": opening an `hs-kv` keyspace writes nothing. A fresh
  store creates exactly one Fjall keyspace, whatever the number of tables.
- **Every operation is translated in `hs-kv`'s Fjall backend**: point reads and writes prefix the
  key, a range is bounded to the keyspace's prefix at whichever end the caller left open (so a
  scan never sees, and a transaction's conflict tracking never covers, another keyspace's keys),
  and results have the prefix stripped. The `KvBackend` contract is unchanged; no consuming
  crate changes; the conformance suite now also checks that keyspaces are independent.
- **Old data directories are read where they are.** A name that already exists as a Fjall
  keyspace of its own is opened as one, unprefixed, as before; only keyspaces that do not exist
  yet go into the shared one. Nothing is migrated, and nothing has to be.
- **A key may be at most 65,535 bytes once prefixed** (Fjall's `u16` key length): a key that
  would be longer is refused as `KeyTooLarge` with the limit that applies to that keyspace. In
  practice keys are tens of bytes.

## Consequences

- A cold first boot on Fjall is about as fast as a warm one (numbers in status 01, 2026-10-01).
- Per-table Fjall options are no longer possible for new tables: every table gets the shared
  keyspace's options (LZ4, key-value separation), which is what every table had anyway. If one
  table ever needs its own options it can be given a Fjall keyspace of its own again, at one
  creation's cost on the first boot.
- All tables share one memtable and one LSM tree: fewer, larger flushes and compactions rather
  than eighty small trees, and the journal is no longer held back by eighty memtables. Every key
  carries its keyspace name, which Fjall's block-level prefix truncation and LZ4 mostly absorb on
  disk.
- `PLAN.md` section 6.5's "one keyspace per table" now describes the logical model, not the
  physical one, for the embedded backend.
