# 0023: Releasing a shard advances its fencing epoch (2026-10-01)

Status: accepted (track 03). Amends RFC 0001 section 6 ("the epoch is incremented on every
acquisition"). Closes the `docs/next-steps.md` gap "A released shard keeps its fencing epoch
until the next owner acquires it".

## Context

A shard's row holds its owner and its epoch. A replica that acquires a shard is handed a
`Fence` at the row's epoch, and every write to the shard's data checks inside its own
transaction that the row still shows that epoch (`Fence::check`). The epoch advanced only on
acquisition. An ordinary release (`ClusterStore::release_shard`: handing a shard to a live
peer, or convergence giving it back) cleared the owner and left the epoch as it was. So between
a release and the next acquisition, a fence the old owner still held passed `Fence::check`
against the ownerless row, and a write it had in flight could land while nobody owned the shard.
The new owner reads the store after it acquires, so such a write was not lost or split, but
it was a write by a replica that no longer owned the shard, which fencing exists to refuse.

The at-once release of a last replica's drain (`release_shards_fenced`, 2026-09-30) already
advanced the epoch, because there no acquisition was coming at all.

## Decision

- **Every release advances the epoch.** `release_shard` writes the ownerless row at
  `epoch + 1`, in the same transaction that clears the owner, and returns the new epoch
  (`None` when the caller was not the owner and nothing changed). A fence from before the
  release fails from the moment the release commits: either the check sees the new epoch, or
  the release's write conflicts the in-flight transaction's commit.
- **A handoff moves the epoch twice**, once at the release and once at the acquisition. Epochs
  are compared for equality and only need to grow; nothing counts them.
- The contract test `store::tests::acquire_then_release_round_trips_epoch` now states the new
  contract; `fence::tests::a_stale_fence_fails_while_the_released_shard_has_no_owner` is the
  window itself (it fails on the old behaviour).
- **Observable.** `KvOwnership::release` logs each release with its new epoch at `debug` (a
  handoff releases hundreds of shards; the count is already
  `hs_cluster_ownership_changes_total{reason="release"}`).

## Consequences

- A write that races a handoff on the old owner now fails its fence with `503` during the
  ownerless interval instead of landing. Decision 0017 already sends such a request on to the
  new owner, waiting out the handoff, so a client sees a slower request, not an error.
- The admin API's shard listing shows a released shard at the advanced epoch (its mock in
  `hs-admin` already modelled it that way).
- Rows written by an older binary need no migration: the next release or acquisition advances
  them.
