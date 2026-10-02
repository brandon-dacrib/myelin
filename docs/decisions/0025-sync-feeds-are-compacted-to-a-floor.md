# 0025: Sync feeds and the hot-room stream are compacted to a floor, and a room update is fanned out in batches (2026-10-02)

Status: accepted (track 05; the setting is in track 13's `hs-config`). Closes the
`docs/next-steps.md` gaps "The owner's session hub writes each member's record and feed entry
one store round trip at a time" and the hot-room stream and feed pruning item.

## Context

`/sync` is built on a per-user, coalesced feed (`PLAN.md` 6.6): the owner of a room writes one
feed entry per active member per update, and a token's `feed_seq` is a position in that feed.
Two things were left open when that landed:

- **Cost.** `SessionHub::apply_room_update` read each member's membership record and appended
  each member's feed entry one store call at a time: on PostgreSQL about ten statements and two
  transactions per member, sequentially, before anyone was woken. Measured in session 12: 8 s
  of the 8.25 s write-to-woken-sync latency in a 303-member room was this, and 32 minutes to
  catch up after 300 joins.
- **Growth.** Coalescing bounds how fast a feed grows between two syncs, not how long it is
  kept: once any device had been handed a token at or past an entry, the entry stayed for ever
  (`room_pos_as_of` may be asked for it by that token). The hot-room stream, one entry per update
  to a room over the fan-out threshold, was never pruned at all.

## Decision

- **A room update's fan-out is a few transactions, not one or two per member.**
  `UserStore::get_memberships` reads every target's record with one multi-get;
  `UserStore::apply_fan_out` writes the records and feed entries in transactions of at most 100
  members, reading the feed pointers, feed heads and device-cursor maxima of a batch with one
  multi-get each. Two summary rows make that possible: `hs_user.feed_heads` (a user's newest
  `feed_seq` and floor, written by every append) and `hs_user.device_cursor_max` (the maximum
  over a user's device cursors, written by `record_device_cursor` when it moves). Both are
  recovered from the rows they summarise when absent, so an existing store needs no migration.
  A batch whose transaction keeps conflicting (its members' syncs recording cursors while it
  ran) is written member by member, as every fan-out used to be; `HS_SYNC_FAN_OUT_UNBATCHED=1`
  makes that the only path, for measurement.
- **Retention is a floor per feed, and one for the hot-room stream.** `compact_feed` moves a
  user's floor up so that at most `keep` entries lie above it; at or below the floor, exactly
  one entry per room stays (the newest), the rest are deleted. The hub compacts a feed once it
  has grown to twice the retention since its floor, after the wake, so a feed is between one
  and two retentions long. The hot-room stream is compacted the same way, by the hub that
  appends to it, with its floor in `hs_user.ephemeral_counters`.
- **Why that is safe for every token.** A kept entry is never rewritten (an append coalesces
  only into an entry above the floor as well as above the maximum device cursor), so it stays
  the room's position as of its own `feed_seq`. A token at or above the floor sees what it did:
  entries above the floor are untouched, and a room's position as of the token is its newest
  entry at or below the token, which is above the floor or the kept one. A token below the floor
  still finds every room that changed after it, because a room with a deleted entry after the
  token has its newest entry at or below the floor kept, and that one is at or after the deleted
  one; what such a token may have lost is the room's position as of itself, and `/sync` then
  resumes from a kept entry no newer than the token, or sends the room whole. Repeat, never skip:
  no special case in `/sync`, and no token is ever refused.
- **The setting is `server.sync`** (`feed_retention_entries`, default 10,000;
  `hot_room_stream_retention_entries`, default 100,000; `0` keeps everything), hot: the hub
  reads it on every room update. `hs serve` logs the values in effect at startup. Under `server`
  rather than a section of its own because the management interface lists its eleven sections
  by name; a `sync` section is a later, cross-track change.
- **Observability.** `hs_user_fan_out_duration_seconds`, `hs_user_fan_out_members_total`,
  `hs_user_fan_out_transactions_total{outcome=batched|fallback}`,
  `hs_user_pruned_entries_total{stream=feed|hot_room_stream}`,
  `hs_user_compactions_total{stream}`.

## Consequences

- A client that has not synced for longer than its feed's retention is sent whole the rooms
  whose position as of its token was compacted away (a `limited` timeline with `prev_batch`), not
  an error. 10,000 entries is at least 10,000 room updates since that device's token, since
  entries above the maximum device cursor coalesce.
- A compaction scans at most 50,000 rows per transaction (`COMPACTION_SCAN_LIMIT`); a feed that
  grew for months before this existed is compacted in pieces, one per fan-out past twice the
  retention.
- Two replicas owning rooms of the same user both append to that user's feed, as before: they
  conflict on the head row and retry. Both may compact the hot-room stream; the second finds
  nothing to do.
- Measured numbers: `docs/status/05-sync.md`, session 14.
