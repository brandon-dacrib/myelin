# 05 Sync: status

Last updated: 2026-10-02 (session 13: `/joined_rooms` read-your-writes. Session 12: RFC 0018, a non-owner's room copy catches up instead of
reloading. Session 11, session 10, session 9, session 8, session 7 and the integration note
follow; sessions 1-6 are preserved unchanged further down.)

## Session 13 (2026-10-02, branch `agent/joined-rooms-rywr`): `/joined_rooms` sees the caller's own writes

Closes the known gap "`TestRoomState` flaps: a room just created can be missing from
`/joined_rooms`" (status 14 session 6), which on 2026-10-01 night also failed `hs-cli`'s
`room_id_uniqueness.rs` on GitHub's arm64 runner on every push to `main` (twenty rooms created at
once, then `/joined_rooms` short one or two of them; never on amd64, never on the desktop).

`UserStore::list_memberships`, which `get_joined_rooms` lists, is written by the session hub off
the registry's global stream, a moment after the event. `/sync` has waited for that moment since
`4e1990f` (`SessionHub::settle_before_read`: everything the registry had published when the
request arrived is consumed before the read, bounded at 500 ms, and in a cluster every peer's
wakes too). `/joined_rooms` now does the same before it reads, one call. Nothing else changed:
the hub, the store and the stream are as they were.

Tested: `cargo test -p hs-user` (unchanged suite) and `cargo test -p hs-cli --test
room_id_uniqueness` three times in a row on the desktop, where it had never failed; the proof is
CI's arm64 leg going green on the merge: it did, on `1248b3b` (run 36957339514, every job green,
the first green `ci` on `main` since `564f540`). The
wait's own tests are session 10's (`a_sync_sent_the_moment_after_a_join_sees_the_join`, and the
hub made to fall behind).

## Session 12 (2026-10-01, branch `agent/rfc-0018`): a replica that does not own a room reads only its new events

Closes the known gap "A non-owner replica reloads a whole room per event to answer `/sync`"
(RFC 0018, now implemented; decision 0022). Joint with track 04: the catch-up itself is in
`hs-room`.

**What it was.** A replica answers `/sync` for rooms it does not own through
`cluster::RoomMirror`, a read-only `RoomActor`. Every time the store's timeline head moved past
it, the mirror ran `RoomActor::load` again: the whole room, per event, on every replica with a
reader in the room.

**What it is now.**

- `hs_room::actor::catch_up` (new module): `RoomActor::catch_up` reads only the `room_timeline`
  rows past the actor's own head and absorbs them in order exactly as `load` does (body and
  flags, state fed to the state store -- so membership and history visibility apply event by
  event -- relations), then re-reads the forward extremities. Everything is checked before
  anything is absorbed; when the copy cannot be advanced from new rows it answers
  `CatchUp::Reload(reason)` and changes nothing.
- A per-room **rewrite counter** (`room_rewrites`, `(RoomSn,) -> i64`), bumped in the same
  transaction by every write that changes rows a copy may already hold: outliers, history placed
  below the head (backfill, a gap filled), a gap closed, a purge, pruned extremities. A copy
  whose counter is behind reloads (`rewritten`); so does one that finds a position missing
  (`position_gap`), a new event with an explicit state (`explicit_state`, a rejoin through
  another server), an unexpected row, or a deleted room (`gone`).
- **Redactions** are applied by the copy: it re-reads the target's stored row when it absorbs
  the redaction, and re-checks a target not yet flagged (the owner rewrites the row a moment
  after the redaction event) on later catch-ups, at most 16 times.
- `RoomMirror` loads a room once, then catches it up; a reload is an `info`/`warn` line with the
  reason. It holds at most 1,024 copies (least recently read dropped; `with_max_rooms`) besides
  the ten-minute idle sweep. A copy is advanced in place, so a reader holding its handle sees the
  room move on, as on the owner.
- **The wake triggers it.** `RoomWake::room_pos` already is the owner's head after the update; the
  receiving hub runs `RoomMirror::prefetch(room, room_pos)` for each room in a batch as tasks,
  waits for them at most 250 ms, then wakes that batch's users; a copy at or past the position
  reads nothing. (Awaiting them inline, as the first version did, let a whole reload of a big
  room hold the peer's mesh request past its 2 s deadline; the request was cancelled, and the
  reload and the wakes with it.) The durable-head check on every read stays.
- **Observability:** `hs_user_mirror_rooms`, `hs_user_mirror_catchup_events_total`,
  `hs_user_mirror_full_reloads_total{reason}` (`first_read`, `rewritten`, `position_gap`,
  `explicit_state`, `unexpected_row`, `regressed`, `error`, `incremental_off`),
  `hs_user_mirror_catchup_duration_seconds{kind=incremental|full}`,
  `hs_user_mirror_wakes_covered_total` (new `hs_user::metrics`, registered by `hs-cli`).
- **Escape hatch:** `HS_SYNC_MIRROR_FULL_RELOAD=1` in a replica's environment turns catch-up off
  (as before; a `warn` at startup). Also the measurement's baseline.

**Tests.** `hs-room`: `actor::catch_up::tests` (five: only the new rows, ending identical to a
fresh load in timeline, state, extremities and every user's visibility; visibility and
membership changes in one batch applied in order; a redaction now or on a later catch-up; a
never-flagged target given up on; a purge, a missing position and a deletion each answer
`Reload` and change nothing). `hs-user` (`cluster::two_replica_tests`, two hubs on one store):
`a_second_wake_for_a_mirrored_room_reads_only_the_new_positions` (the mirror's loader count stays
1 across two wakes; 3 new events then 5 caught up),
`a_visibility_change_mid_stream_is_honoured_on_the_other_replica` (bob on B is not sent what
came before his join into a `joined` room, nor what came after his leave, and every change
reached B by catch-up), `a_rewrite_on_the_owner_makes_the_copy_reload`,
`the_mirror_holds_at_most_its_bound_of_rooms`, and the old reload test rewritten for in-place
catch-up. With catch-up off (the old behaviour) the first two fail: loader count 2 against 1
after the first wake, and 3 whole loads against 1.

**Measured** (`crates/hs-cli/tests/cluster_mirror.rs`, new: three real `hs serve` on one
PostgreSQL 17; A owns the rooms; B1 runs with `HS_SYNC_MIRROR_FULL_RELOAD=1`, B2 as shipped; bob
long-polls on B1 and carol on B2 while alice sends through A, so both see the same events at
the same time). First, a debug build on the shared desktop at load 15-20 (and before the
250 ms prefetch bound), so absolute numbers are inflated; the ratio is the point. Mirror work
per event is the
`hs_user_mirror_catchup_duration_seconds` sum over the phase divided by the messages:

| Room | B1, whole reload (before) | B2, incremental (after) |
|---|---|---|
| small (3 members, ~10 events), 20 messages | 551 ms/event | 70 ms/event |
| 300 messages, 33 members, 20 messages | 3,526 ms/event | 86 ms/event |

B2 loaded no room whole in either phase and its cost per event barely moves with the room's
size; B1's grows with it.

**The 2,000-event rooms, release build** (PostgreSQL 17 in Docker with `fsync=off`; the desktop
quieter than above), 100 messages per phase, two runs:

| Room | B1, whole reload (before) | B2, incremental (after) |
|---|---|---|
| small (3 members) | 45.7 / 43.9 ms/event; sync p50 281 / 259 ms | 4.2 / 3.7 ms/event; p50 246 / 228 ms |
| 2,000 messages, 53 members | 989 ms/event; sync p50 2.26 s, p95 2.69 s | 3.4 ms/event; p50 1.54 s, p95 1.83 s |
| **2,000 messages, 303 members** (the brief's room) | **2,054 ms/event**; sync p50 9.57 s, p95 15.4 s | **4.9 ms/event**; p50 8.25 s, p95 11.8 s |

B2 loaded no room whole (100 catch-ups of 100 events in every phase), and its work per event
is the same, 3.4-4.9 ms, whatever the room's size; B1's grows with the room, to 420 times B2's
in the 303-member room. What is left of the latency on B2 is the owner: its hub writes every
member's record and feed entry, one at a time, before it sends the wake -- about 1.5 s for 53
members and 8 s for 303 (the owner's fan-out, below). Session 7's ~150 ms was a two-member room.
Earlier attempts at the 303-member room on the loaded desktop never reached the message phase
(the owner's hub needed about a minute per update after the joins); on the quieter machine it
needed 32 minutes to catch up after the 300 joins.

**Found with no row: the owner's fan-out.** In the debug run the write-to-woken-sync latency
was the same on B1 and B2 (p50 2.6-2.8 s in the small room, 14 s in the 33-member room): what
dominated it was the owner's session hub, not the reader. The hub writes each update's
membership records and feed entries one member at a time, several store round trips each
(`hub::apply_room_update`), so every update costs the owner O(members) sequential PostgreSQL
round trips before anyone is woken; with 30 members on the loaded desktop it ran minutes behind
the room, and a reader's own join reached its records only when the hub got there. Added to the
known-gaps table.

**Also learned on the way** (test harness, not server): `createRoom` places a room on whichever
replica owns its minted id's shard, so the test makes rooms until A owns one; with 500 ms
heartbeats and 3 s leases a loaded machine moved shards mid-test, so it uses 2 s and 30 s.

**Left.** `RoomRegistry::read_room` (search on a non-owner) still loads a room whole per
request; it could use the mirror. A future path that rewrites a room's existing rows must bump
the rewrite counter (decision 0022 says so). The two-pod run has not seen this yet.

**Files.** `crates/hs-room/src/actor/catch_up.rs` (new), `crates/hs-room/src/actor.rs`,
`crates/hs-room/src/actor/{admin_ops,gaps}.rs`, `crates/hs-room/src/persist.rs`,
`crates/hs-user/src/{cluster,hub,metrics,lib}.rs`, `crates/hs-user/Cargo.toml`
(`prometheus-client`, already a workspace dependency), `crates/hs-cli/src/{sync_cluster,serve}.rs`,
`crates/hs-cli/tests/cluster_mirror.rs` (new).

**Verify.** `cargo test -p hs-room --lib catch_up`; `cargo test -p hs-user --lib cluster`;
`HS_CLUSTER_TEST_POSTGRES_DSN=... cargo test -p hs-cli --test cluster_mirror -- --nocapture`
(defaults are a quick 150-message, 10-member room; the sizes above and below are set with
`HS_MIRROR_BENCH_EVENTS`, `HS_MIRROR_BENCH_MEMBERS`, `HS_MIRROR_BENCH_MESSAGES`).

**Decisions made.** Decision 0022 (the rewrite counter rather than enumerating changes;
redactions applied by the copy; the wake's existing `room_pos` as the head; 1,024 copies; the
escape hatch).
Last updated: 2026-10-01 (session 12: the legacy event stream. Session 11: three known gaps and a
hot-room bug. Session 10,
session 9, session 8, session 7 and the integration note follow; sessions 1-6 are preserved
unchanged further down.)

## Session 12 (2026-10-01, branch `agent/sytest-client`): the legacy event stream

Row "The legacy `GET /events` stream is unimplemented" (Sytest's first run: 21 fixture
failures, its helpers wait on `/events` even with `--exclude-deprecated`).

- **`GET /events?from=&timeout=&room_id=`** (`hs_user::routes::events::get_events`) is an
  incremental `/sync` (`sync::build`) from `from`, flattened by `events_chunk` into one `chunk`:
  every joined room's new timeline events, typing and receipts, the caller's own invites (the
  `m.room.member` of `invite_state`) and left rooms' timelines, each with `room_id`, then
  presence (with the old `content.user_id`). `start` is `from`, `end` the sync's `next_batch`,
  so `/events` and `/sync` tokens are interchangeable. It long-polls up to `timeout` (default 0,
  at most 60 s); a `timeout` that is not a number is `400 M_INVALID_PARAM`. No `from`, or a
  `from` that is not a stream token, starts from now (an empty initial sync's token). Up to 100
  events per room per answer.
- **`GET /initialSync?limit=&archived=`** (`get_initial_sync`) is an initial sync rearranged by
  `initial_sync_body`: `rooms` (joined, invited with `invite`, and left with `archived=true`),
  each with `membership`, `messages {chunk, start, end}` and `state` (the state before the
  timeline, then the timeline's state events, latest per key), plus `presence`,
  `account_data` and `end`.
- **`GET /rooms/{roomId}/initialSync`** is `hs-room`'s (`routes::query::get_room_initial_sync`):
  `membership`, `state` through `full_state_for_reader`, the newest `limit` messages oldest first
  through the same page `/messages` reads, `visibility` from the directory; a member, a past
  member, or anyone for a `world_readable` room, otherwise `403`.
- Both build the sync **with no device**: `/events` takes nothing from a device's to-device
  queue and moves no device's position, so Sytest's helpers polling `/events` beside `/sync` lose
  nothing. The stream records its own feed cursor under `\u{1}hs-user:legacy-events` (session
  11's reason: without a cursor the feed coalesces the entry a token points at, and the next
  read sees nothing).
- Not done: `GET /events?room_id=` for a room the caller is not in (the old "peek" at a
  `world_readable` room) answers an empty `chunk`; a deprecated Sytest test or two expect events.

Tests: `routes::events::tests` (the flattening, the old initial-sync shape, a number that is not
one, and through the real feed: each new message once, with its room, and `/initialSync`
listing the room), `hs-room`'s `room_initial_sync_answers_members_and_world_readable_strangers_only`,
and the real binary, `hs-cli/tests/legacy_events.rs`: `/events` with no `from`, a message after
it with its room, a long poll that returns when a message is sent rather than at its timeout,
`timeout=hello` refused, `/initialSync` and the room's `/initialSync`. All were 404 before.

Sytest, the whole client-server group: **319 of 542 → 362 of 543** (the grouping script counts one
more test now), and the whole suite **407 → 458 of 772**, with all three of the branch's rows and
its two fixes (`docs/status/sytest/2026-10-01b-*`, at `51c0b68`; nothing that passed before
fails). The 21 "fixture failed ... 404 /events" failures are gone. Of the 51 gained, 23 are the
guest group and 7 the 3PID group (status 07, session 11); the others are
tests about something else whose fixtures or waits read `/events` (`local_user_fixture(with_events
=> 1)`, `await_event_for`): typing (four, "Typing notifications don't leak" among them),
presence (five), "Events come down the correct room", two inbound-federation tests, three
appservice ones, "Rooms can be created with an initial invite list (SYN-205)", a device-list
rejoin and a v3 invite rejection over federation. `/events` and `/initialSync` themselves are
deprecated and excluded (`--exclude-deprecated`), so none of their own tests ran.

Found on the way: a backward `/messages` page from a `/sync` `next_batch` started below the
newest event the sync covered (`hs-room`'s `get_messages`, for a token the global resolver maps);
Sytest's `matrix_get_room_messages` does exactly that and so missed a message the sync had just
shown. Fixed: a backward page from a resolved sync position starts just above it, a forward one
after it (unchanged). `tests/sync_scenario.rs::messages_accepts_a_token_minted_by_sync_in_both_
directions` had asserted the old behaviour and now asserts that the page begins with "newer
message".

## Session 11 (2026-09-30, branch `agent/user-gaps`): three known gaps, and hot rooms that repeated themselves

Three rows of the known-gaps table in `docs/next-steps.md`, all in this crate, and one bug
found on the way that had no row and was worse than any of them. One commit each.

**1. A requester with no device records a feed cursor.** (Row "A requester with no device
never records a feed cursor".) `/sync` recorded the device cursor -- the bound that stops the
feed coalescing an entry somebody has been handed (`store::tables` module docs) -- only when
the requester had a device. An appservice acting as one of its users through its `as_token`
without `device_id` (a bridge puppet) has none, so the entry its token pointed at went on
absorbing every later update to that room, and the next incremental sync saw no change, for
ever. Now `sync::cursor_device_id` gives such a requester a key of its own,
`DEVICELESS_CURSOR_KEY` (`"\u{1}hs-user:no-device"`), under its own user id: cursors are kept
per `(user, device)` and only the per-user maximum is read, so two appservice users cannot
collide, and a real device of the same name would only share a cursor whose maximum is what
matters anyway. Both places that record a cursor (`sync::build`, before reading;
`routes::sync`, after) use it. A device-less sync is a `debug` line.
Test: `routes::sync::tests::a_requester_with_no_device_sees_each_new_event_once` -- through the
real `GET /sync` handler, two masquerading puppets (an `AppserviceIdentity`, no device): initial
sync, a message, the incremental sync carries exactly it, the next carries nothing. Without the
fix: `rooms: {}` on the second sync.

**2. A hot room joined after the token arrives whole -- and a hot room no longer repeats.**
(Row "A hot room joined after the token is resumed from the join, not sent whole".) A room
above the fan-out threshold (500 members by default; `hub` module docs) gets no feed entries,
so `resume_mode` could not tell "joined after the token" from "member all along" and resumed
both from the user's own membership event. Looking at that turned up the bug with no row:
**every incremental sync of a member of a hot room re-sent everything since their own
membership event** (or, past `limit`, the newest events with `limited: true`), and the
long-poll returned at once every time, because `has_new_data` asked "does the room have
anything after this member's membership event", which is true for ever once anybody has spoken.
A client in any room over 500 members spun on `/sync`. Shown before the fix by an experiment
(threshold 1, two members): the second incremental sync repeated the message, in 3 ms against a
50 ms timeout.

The fix gives hot rooms what the feed gives cold ones: a position as of a token.

- **The hot-room stream** (`hs_user.hot_positions`, key `(room_id, hot_seq)`, value `room_pos`):
  one entry per update to a hot room, whatever its size, at the next position of one
  server-wide counter (`ephemeral_counters`/`hot_positions`, `atomic_add` in the same
  serializable transaction, so a reader that sees the counter at `n` sees every entry up to
  `n`). Written by the room owner's hub after the membership records and before anyone is
  woken (`apply_room_update`; the other order could resume a member who had just joined from
  their own join). `UserStore::{append_hot_position, latest_hot_seq, hot_room_pos_as_of,
  latest_hot_seq_of_room}`.
- **The token carries `hot_seq`** (`SyncToken`, wire version 4, 73 bytes). A version-3 token is
  still accepted, decoding with `hot_seq` 0: there are real clients now, and a `400` on `since`
  sends some of them back to an initial sync. The cost is a hot room sent whole once.
- **`resume_mode`** takes the newer of the feed's position as of `feed_seq` and the hot-room
  stream's as of `hot_seq`, then the same `was_joined_at` question it already asked for an
  accepted invitation -- so somebody who joined a hot room after the token is sent it as an
  initial sync would (state, recent timeline, `limited` as the room's size makes it). No
  position at all means new to this client: sent whole. A hot room with no entry on the stream
  at all (a server upgraded from before it, quiet since) is resumed from its head: nothing has
  happened there the client could have missed.
- **The batch bound** for a hot room is its stream position as of the token's `hot_seq`
  (it used to be read live), and **the long-poll** wakes for a hot room only when its newest
  stream entry is past the token's.
- `FeedTokenResolver` (a sync token used as `/messages`' `from`) takes the same newer-of-two
  baseline.
- The hub logs at `info` when a room crosses the threshold in either direction ("a room crossed
  the fan-out threshold; its members' records now say so", with the member count and
  threshold).

Tests (each fails on the old logic; checked by swapping the old `resume_mode` fallback, live
bound and wake check back in):
`sync::tests::a_hot_room_joined_after_the_token_arrives_whole` (threshold 1; bob's next sync
carries `m.room.create`, power levels, join rules, name and all three memberships, the message
after his join, and alice in `device_lists.changed`; then only "welcome", not limited, no state;
then nothing. Old: "m.room.create never reached bob"),
`a_hot_room_sends_each_event_once_and_lets_a_long_poll_wait` (two messages once, then an empty
answer after the full 300 ms poll, then the third only. Old: "nothing is repeated" failed),
`a_hot_room_from_before_the_stream_is_not_repeated` (a hot record with no stream entry: nothing
sent, the poll waits. Old: answered in 1 ms with the room),
`store::tables::tests::the_hot_room_stream_answers_a_rooms_position_as_of_any_point`,
`token::tests::a_version_3_token_still_decodes_with_no_hot_position`, and the token round-trip
property test over nine fields.

**3. The user directory reads an index, not the rooms.** (Row "User-directory scope is
computed by walking rooms on every search".) `users_visible_in_directory_to` loaded every room
the searcher is joined to and every public room, and read each one's members through the room
actor, on every search. Now:

- **The index** (`hs_user.room_members`, key `(room_id, user_id)`): each room's joined members,
  with a marker row (empty user id) that says the room is indexed. `UserStore::
  {index_room_members_if_absent, apply_room_member_changes, room_member_ids,
  forget_room_members}`. A whole index and its marker are one transaction, so a room is indexed
  wholly or not at all, and a second indexer (the hub racing a search on another replica)
  leaves the first one's rows alone.
- **Kept current by the hub** from the room updates it already applies
  (`index_members_for_directory`): each member an update changed is added or removed by what
  the room says their membership is now (the member list `apply_room_update` reads anyway); a
  room the index has nothing for is indexed whole from that list. A deleted room's rows go
  (`apply_update_for_a_gone_room`). A room going public or private needs nothing new: the public
  room list the hub keeps already follows `m.room.join_rules`.
- **The search** is: the searcher's joined rooms (their membership records) plus the public
  rooms (the list), each one's members one range read. It first waits for the hub to catch up
  (`settle_before_read`, as `/sync` does), so somebody who has just joined a room finds its
  members.
- **The rebuild** is the old walk, per room and once: a room the index has nothing for (its
  last update came before the index existed) is read from the room and indexed by the first
  search that needs it. `SessionHub::directory_rooms_walked()` counts those, and each is an
  `info` line ("read a room's members for the user directory, which had no index of it yet").
  Not on boot, and not "when the table is empty": a quiet room is indexed when a search first
  needs it, and an active one by its next update, whichever comes first.
- Semantics unchanged: shared joined rooms plus public (`join_rule: public`) rooms' joined
  members, world-readable history not counted, as before.

Tests: `hub::tests::a_directory_search_answers_from_the_index_without_loading_any_room` (the
index seeded for a room the registry does not have: the answer comes from the index alone; the
old walk answered nobody), `a_room_from_before_the_index_is_read_once_then_kept_current` (one
walk for two searches, then a join kept current with no second walk; fails on the old code at the
walk count), `the_directory_follows_membership_from_its_index_without_reading_rooms` (join,
leave, a public room made invite-only; `directory_rooms_walked() == 0`; passes on the old walk
too, which is the point: same answers), `store::tables::tests::
the_directory_index_is_written_whole_once_then_changed`. The real-server directory test
(`hs-cli/tests/e2e.rs::the_user_directory_shows_a_searcher_only_who_they_could_already_see`)
passes unchanged.

**Timing** (`hub::tests::directory_search_timing_in_a_public_room_of_5000`, `#[ignore]`d; one
public room of 5,001 joined members, searched by somebody outside it, 20 rounds each, in-memory
backend, `--release` on the desktop):

| per search | release | debug |
|---|---|---|
| before: the room resident, members read through the actor | 2.3 ms | 25 ms |
| before: the room not resident (loaded, then read) | 125 ms | -- |
| after: from the index | 2.7 ms | 25 ms |

Honestly: against a room that is resident and idle, the index is no faster -- both read 5,000
rows from memory. What it removes is the room: a search no longer loads a room that is not
resident (after a restart, or on a replica reading through its mirror, which reloads the whole
room), and no longer queues on a busy public room's actor behind its writes. On Fjall or
PostgreSQL the index is a range read of one key prefix; not measured there.

**Verification.** `cargo fmt --all --check`; `cargo clippy -p hs-user -p hs-cli --all-targets
-- -D warnings` clean; `cargo test -p hs-user`: 162 lib (+1 ignored timing) + 6 scenario, from
153 + 6. The 13 `crates/hs-cli/tests/` files that touch `/sync` or the user directory
(`admin_areas`, `admin_rooms`, `bridge_offerings`, `cluster_edus`, `cluster_ephemeral`, `e2e`,
`federation_catch_up`, `federation_edus`, `federation_membership`, `federation_restart`,
`federation_two_servers`, `invites_and_notices`, `migration`): all pass. In the first run three
of `e2e.rs`'s real-binary tests timed out waiting for the binary to boot (load average 21 on the
desktop's 10 cores, several agents building); run again on their own they passed. The two
cluster files print `SKIP` without `HS_CLUSTER_TEST_POSTGRES_DSN`, which this session did not
set (the gate's PostgreSQL containers were left alone).

**Decisions made.**
- A device-less requester's cursor is a synthetic device key per user, not a cursor keyed on
  the token: cursors exist only to bound coalescing, and only their per-user maximum is read.
- Hot rooms get a server-wide position stream rather than a per-room position in the token
  (which would grow with the account) or a feed entry per member (which is what hot rooms
  exist to avoid). One counter for all hot rooms means their writes serialize on it; hot rooms
  are rare (over 500 members), so this is accepted.
- `SyncToken` version 4 still reads version 3: the first format change made with real clients
  holding tokens.
- The directory index is per room (its joined members), not per searcher: a
  `(searcher, visible user)` table is quadratic in a big public room. The rebuild is lazy and per
  room rather than a boot-time pass.

**Interfaces provided (new).** `hs_user::sync::{cursor_device_id, DEVICELESS_CURSOR_KEY}`;
`SyncToken::hot_seq`; `UserStore::{append_hot_position, latest_hot_seq, hot_room_pos_as_of,
latest_hot_seq_of_room, index_room_members_if_absent, apply_room_member_changes,
room_member_ids, forget_room_members}`; `SessionHub::directory_rooms_walked`. Two new keyspaces,
`hs_user.hot_positions` and `hs_user.room_members` (a first boot creates two more, see the
"first boot takes about five seconds" row). No other crate changes; `hs-cli` needs no wiring.

**Left.**
- The hot-room stream is never pruned (one row per event in a hot room, as the feed is never
  pruned either); a retention pass is the same work for both.
- `directory_rooms_walked` and the threshold crossing are logs and a counter on the hub, not
  Prometheus metrics: `hs-cli` registers metrics, and it was not in this session's scope.
- `users_sharing_room_with` (presence and device-list scope, on every `/sync`) still reads each
  shared room through its actor; it could read the same index, and is the next thing worth
  measuring.
- The hot-room path has no real-binary test (a 500-member room through HTTP); the unit tests
  use a threshold of one.

**Files.** `crates/hs-user/src/sync/mod.rs` (`cursor_device_id`, `resume_mode`, the hot bound
and wake), `routes/sync.rs` (the cursor, the device-less test), `token.rs` (`hot_seq`, v4),
`store/mod.rs` and `store/tables.rs` (the hot-room stream, the directory index), `hub.rs`
(writing both, the directory search, the threshold log).

## Session 10 (2026-09-30, branch `agent/ci-flakes`): the two races that kept `main` red

`main` failed CI on every push after `d6b3cd7`: two real-binary tests in `crates/hs-cli/tests/`
failed on GitHub's loaded amd64 runner. Both were server races in this crate, and both are
fixed in the server, not by waiting in the tests.

**1. `PUT /typing` right after a join answered `403`.**
(`appservice_ephemeral.rs::a_bridge_is_sent_ephemeral_data_once_across_a_restart_and_not_while_paused`.)
`put_typing` gated on the user store's membership record, which the session hub writes off the
registry's global stream a moment *after* the room accepted the join -- the lag `/sync`
already waits out. `/receipt` and `/read_markers` (`routes::receipts::require_joined`) had the
same hole; nothing else in `hs-user` gates on store membership. New
`SessionHub::is_joined(user, room, at_most)`: the store record first (the usual, cheap
answer); if it does not say `join`, the room's own current `m.room.member` state (written the
instant the join was accepted; through the mirror on a non-owner replica); if that does not
say `join` either, `settle_before_read` -- the same bounded wait `/sync` makes,
`READ_YOUR_WRITES_WAIT`, 500 ms, now `pub(crate)` -- and the store again. A room that does not
exist has no joined members (`403`, as before). The three routes use it.

**2. `/sync` answered `404 room not found` after an admin room deletion.**
(`admin_rooms.rs::a_deleted_room_empties_moves_its_members_and_cannot_be_joined`.) The delete
(`hs-room`'s `admin::content::delete_room`) makes each local member leave and then purges the
room (`delete_everything`, `forget_resident`) in one go. A hub a moment behind read the leaves
after the room was gone: `apply_room_update` could not load it, logged a warning and dropped
them, so every member's record still said `join` for a room the registry no longer had, and
the per-room loop in `sync::build` turned the `RoomNotFound` into the whole response's error.
The same happened, without the hub lagging, to any incremental sync whose feed still held the
deletion's leave once the purge had run. Fixed at both ends:

- **The hub applies a gone room's update from the update itself**
  (`apply_update_for_a_gone_room`): each `membership_delta` is written to the member's record
  with a feed entry (unless the room was hot), the member is woken, and the room's public
  directory row is removed. Logged at `info`.
- **`/sync` never fails because one room is gone.** A room the registry cannot load is
  reported in `rooms.leave` with an empty timeline and state in an incremental sync (so a client
  that still lists it drops it, once), and left out of an initial sync unless the filter asks
  for left rooms. `resume_mode` takes the handle the loop already loaded instead of loading the
  room again; the long-poll's hot-room check skips a gone room; and the walks over a user's
  rooms -- `users_sharing_room_with` (presence, device lists), `users_visible_in_directory_to`,
  and the three member lookups in `sync::build` -- go through new
  `SessionHub::joined_member_ids_if_present`, where a gone room has no members.
  `UserError::is_room_not_found` names the case.

**Observability.** `wait_for_consumed` (behind `/sync`'s wait and now the membership gates')
logs a `warn` when the hub has not caught up within its bound ("the session hub did not catch
up with the room stream in time; reading anyway", with `waited_for`, `consumed`, `waited_ms`):
before, a read-your-writes miss was silent. `is_joined` logs at `debug` when the store was
behind and the room or the wait answered; a gone room's update is an `info` line; a sync
reporting a gone room as left is a `debug` line.

**Tests** (each fails without its fix; checked by disabling each fix in turn):

- `routes::typing::tests::a_join_the_hub_has_not_consumed_yet_still_lets_the_member_type`:
  nothing consumes the room stream, so the store has no record of the creator's or the joiner's
  membership; both may type, a stranger may not. Without the fix: `403 must be a joined member`.
- `routes::typing::tests::typing_in_a_room_that_does_not_exist_is_forbidden`.
- `routes::receipts::tests::a_join_the_hub_has_not_consumed_yet_still_takes_receipts`: a
  receipt and a fully-read marker in a room whose creation the hub has not seen.
- `sync::tests::a_deleted_room_does_not_fail_a_members_sync_and_is_reported_as_left`: the
  hub's lag held open by feeding it by hand; bob leaves, the room is purged and forgotten,
  then both members' initial syncs and bob's incremental sync succeed, the late leave is
  applied, bob's next sync carries the room in `leave`, and the one after does not. Without
  the sync fix: `room !...:sync.test not found` (CI's message); without the hub fix the late
  leave fails with `RoomNotFound`.
- `cargo test -p hs-user`: 152 lib + 6 scenario (from 148 + 6).
- Real binary, debug, under load: the whole `appservice_ephemeral` and `admin_rooms` test files,
  12 runs each in a row, the two loops running at the same time beside six CPU burners (load
  average about 10 on the desktop's 10 cores): **12/12 and 12/12 pass**, 26-41 s a run. The
  failures were not reproduced on this desktop before the fix either (CI's runner is slower),
  so the unit tests above, which hold the race open, are the proof; these runs show nothing
  else broke.

**Files.** `crates/hs-user/src/hub.rs` (`is_joined`, `joined_member_ids_if_present`,
`apply_update_for_a_gone_room`, the `wait_for_consumed` warning), `error.rs`
(`is_room_not_found`), `routes/typing.rs`, `routes/receipts.rs`, `sync/mod.rs`.

**Verification**: `cargo fmt --all --check`; `cargo clippy -p hs-user -p hs-cli --all-targets
-- -D warnings`; `cargo test -p hs-user`; `cargo test -p hs-cli --test appservice_ephemeral`
and `--test admin_rooms`, repeatedly.

## Session 9 (2026-09-30, branch `agent/ephemeral-replicas`): typing, receipts and presence cross replicas

**The gap** (`docs/next-steps.md`, "Known gaps"): typing was each replica's memory; receipts
and presence were durable, but a replica read a room's receipts (or a user's presence) from
the store once and then served its cache. A user on replica B saw no typing, and no later
receipt or presence change, from a user on replica A. Decision 0018 records the choice.

**The mechanism: the wake batch carries it.** Session 7's `user.wake` message (a
`hs_user::cluster::WakeBatch`, one pump per peer, coalesced per mesh round trip, to every
live replica) gained an `ephemeral: Vec<EphemeralUpdate>` field. The alternatives -- a
second mesh message, or a per-room pull from the owner on every `/sync` -- were a second
pump for nothing, and RFC 0018's rejected fan-out respectively. What travels:

- **Typing, whole**: `Typing { room_id, user_id, typing, timeout_ms }`. Typing is in no store
  and stays that way. The receiver's `TypingRegistry::set` takes it with the timeout and
  expires it on its own clock, so a lapse shows on every replica within its own 500 ms
  re-check, and a stop is a second update. Not through the database, as the gap row asked.
- **Receipts and presence, as a hint to reread**: `Receipt { room_id, seq }` and
  `Presence { user_id, seq }`. The data is in the shared store already, written by the replica
  that took it; only the peer's cache is behind. New `ReceiptRegistry::forget(room, seq)` and
  `PresenceRegistry::forget(user, seq)` drop the cached room or user (a cached "no record"
  included) so the next read is from the store, and `Stamps::observe` the writer's stamp so
  nothing stamped here afterwards is older. Rereading, not re-sending: one source of truth,
  one code path for a restart and for a peer.
- **Who publishes**: whichever replica changed the state, not only a room's owner.
  `SessionHub::set_typing`, `set_receipt`, `set_presence`, `touch_presence` (on a change),
  the join restamp in `apply_room_update`, and `receive_edu` (an EDU from another server is
  applied on the replica whose transaction it arrived in, and the others were never told) all
  call `publish_ephemeral`. A receiver (`SessionHub::receive_wakes` ->
  `apply_ephemeral`) applies and wakes -- the room's joined members for typing and receipts
  (read through the mirror on a non-owner), the presence audience and the user for presence
  -- and never re-publishes, so there is no loop. Typing and receipts are `/rooms/...`
  requests and so always land on the room's owner (the gate forwards them); presence and EDUs
  land wherever they arrive.
- **`SessionCluster::publish_ephemeral`** is the one new trait method; `hs-cli`'s
  `MeshSessionCluster` queues it into the same per-peer pump (`Outbound::{Wake, Ephemeral}`),
  the pump folds both into one batch (`WakeBatch::push_ephemeral` coalesces: a later typing
  update for the same room and user replaces the earlier one; a second receipt hint for a
  room, or presence hint for a user, keeps the higher stamp). A batch with only ephemeral
  updates is a batch (`is_empty` knows), and moves no consumed mark.
- **Observability**: `hs_cluster_ephemeral_updates_total{kind="typing|receipt|presence",
  direction="sent|received"}` in `hs-cli`'s `sync_cluster` (`SyncClusterMetrics`, registered by
  `install`, so single-node registers nothing). `sent` is counted once the peer has answered
  200, so a sender's count never exceeds the receiver's. Plus `tracing::debug!` per applied
  update (`"applying a peer's ephemeral update"`) and the batch log lines now carry
  `ephemeral=`. The install line says "and typing, receipts and presence cross it", which the
  real-binary test checks in both logs.

**What it does not do.** No read-your-writes wait for ephemeral data across replicas:
`settle_before_read` waits on registry-stream numbers and these are not on it; the hint is on
the peer within a millisecond, before any `/sync` there can be built, and a long-poll there is
woken by it. Best effort like the wake: a lost hint leaves a peer serving its cache until the
next one, a lost typing update leaves a peer not knowing until the client's next `PUT` (every
few seconds, from real clients). Stamps are now compared across replicas (a client's token
holds the largest it saw from any replica); the hints carry the writer's stamp and the
receiver observes it, so the counters converge with traffic, but a replica whose clock is
behind by more than the gap between two changes could stamp below a token. NTP is assumed,
as the restart-safety of stamps already assumed. A replica that starts after a typing began
has no copy until the next `PUT`. A handoff mid-typing loses nothing: every replica already
holds every typing entry, the next `PUT` lands on the new owner and is published from there,
and applying the same update twice is an idempotent map insert.

**Tests.**

- `crates/hs-cli/tests/cluster_ephemeral.rs` (new; two real `hs serve` processes on
  PostgreSQL, `SKIP` without `HS_CLUSTER_TEST_POSTGRES_DSN`; `pool_size: 8`):
  `typing_receipts_and_presence_cross_two_replicas_in_both_directions`. Alice registers on A
  and bob on B, one room (created on A, joined through B; the gate forwards both to whichever
  replica the room hashes to), each polling `/sync` with `set_presence=offline` on their own
  replica. Every check polls one-second long-polls until a condition, 15 s deadline, never a
  fixed sleep. Typing A->B (start, then stop), typing B->A with a 1.5 s timeout lapsing on A
  by itself (bounded at 10 s, measured under 2); a receipt on the first message A->B, then a
  receipt on the second A->B (the "not served from the cache" case: B had the room's receipts
  loaded), then B->A; presence A->B twice (the second with B's cache warm), then B->A; a
  final `timeout=0` sync on each side carries none of it again; and per kind, what A sent
  equals what B received and vice versa with a non-zero total (typing and receipts are only
  ever sent by the owner; presence by both). **5 of 5 runs pass, 28-30 s each** on the
  desktop's PostgreSQL 17 at `127.0.0.1:5462`, debug binaries. Mutant: with the hub's
  `publish_ephemeral` a no-op it fails at the first check whose direction crosses the mesh
  (which one depends on which replica the room hashed to; seen at "bob typing (set on B) in
  alice's sync on A", 22 s).
- `crates/hs-user/src/cluster.rs`, `two_replica_tests` (fake cluster, shared memory store, A
  owns everything): `typing_on_one_replica_is_in_a_long_poll_on_the_other_and_goes_away_when_it_stops`
  (woken under 400 ms, the stop too, a 200 ms timeout lapses on B within the re-check, B
  publishes nothing back), `a_receipt_on_one_replica_is_in_the_next_sync_on_the_other_and_not_served_stale`
  (with the fake cluster muted the receipt does *not* show on B -- the gap, in the test --
  then two receipts in a row each show, each replacing the last, none sent twice),
  `a_presence_change_on_one_replica_reaches_a_room_mate_on_the_other` (two changes A->B
  with B's cache warm, one B->A, not sent twice). `cluster::tests`: JSON round trip with the
  new field and a pre-field batch parsing, `push_ephemeral`'s coalescing.
- `receipts::tests::a_forgotten_room_is_read_from_the_store_again`,
  `presence::tests::a_forgotten_user_is_read_from_the_store_again`: two registries over one
  store as two replicas; the second serves its copy until told to forget, then reads the
  store, keeps the writer's stamp and stamps past it.
- `hs_cli::sync_cluster::tests::the_peer_handler_applies_a_batchs_ephemeral_updates_and_counts_them`.
- `cargo test -p hs-user`: 146 lib + 6 scenario (from 140 + 6). `cargo test -p hs-cli --test
  cluster_edus --test cluster_admin --test cluster_ephemeral` with the DSN: see the commit.

**Files.** `crates/hs-user/src/cluster.rs` (`EphemeralUpdate`, `WakeBatch::{ephemeral,
push_ephemeral}`, `SessionCluster::publish_ephemeral`, the fake cluster, tests);
`hub.rs` (`apply_ephemeral`, `wake_room_members`, `wake_presence_audience`,
`publish_ephemeral`, the six publish sites); `receipts.rs`, `presence.rs` (`forget`;
`touch`/`restamp` return the stamp as `Option<u64>`); `crates/hs-cli/src/sync_cluster.rs`
(`Outbound`, `SyncClusterMetrics`, the pump and handler; `install` takes `&Metrics`);
`serve.rs` (one argument); `crates/hs-cli/tests/cluster_ephemeral.rs`;
`docs/decisions/0018-ephemeral-state-rides-the-wake-batch.md`; `docs/scaling.md` (the row
and the honest-status bullet that said this was not built).

**Verification**: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D
warnings`; `cargo test -p hs-user`; `HS_CLUSTER_TEST_POSTGRES_DSN=... cargo test -p hs-cli
--test cluster_ephemeral --test cluster_edus --test cluster_admin`.

## Session 8 (2026-09-30, branch `agent/sync-dup`): a batch and its token describe the same point

**The gap** (`docs/next-steps.md`, "Known gaps"): `/sync` could repeat an event across two
consecutive batches. Seen on 2026-09-27 with appservice-sent notices, and tolerated by
`crates/hs-cli/tests/bridge_offerings.rs`, whose client de-duplicated by event id.

**The mechanism** was neither of the two the gap row guessed at exactly, but the second one:
the `next_batch` token's feed position *was* taken before the batch was read (the fix pinned
by `an_event_that_arrives_while_a_sync_is_in_flight_is_not_lost` moved it there so that
nothing could be reported as consumed without being sent), but each room's
timeline was then read from its resume position to the room's *live end*, with no upper
bound. An event landing in the window between fixing the token and reading the room was in
the batch; its feed entry, written after the token was fixed and after the device cursor had
pinned the older entry, was past the token; so the next sync, resuming from the pinned
entry's position, sent it again. Every `await` between the two reads is a place for it to
land -- the cursor write, `feed_since`, three membership listings, the typing and receipt
sweeps, `get_membership`, `room_pos_as_of`, the account-data read -- and a writer racing the
loop in-process hit it on more than half of its events. Initial syncs had the same window
(their rooms were read from the live end too), so the repeat could also be between the
initial batch and the first incremental one. The same window let a room joined during
assembly be sent whole in that batch and whole again in the next.

**The fix** (`crates/hs-user/src/sync/mod.rs`, `store/mod.rs`, `store/tables.rs`):

- A batch's rooms are those with a feed entry at or before its token: `feed_since(baseline)`
  is filtered to `feed_seq <= new_feed_seq`. An entry past the token is the next batch's.
- Each room's timeline stops at the position its feed entry had as of the token.
  `UserStore::room_pos_at_token(user, room, as_of)` is that position: the room's newest entry
  when that entry is at or before `as_of` (`current_feed_entry`, a keyed read through the
  existing `feed_by_room` pointer, one snapshot for pointer and row), and the existing
  `room_pos_as_of` feed walk only when the room has moved on past the token, which is the
  rare case this bug is about. The value is frozen by the device cursor `build` already
  records before reading. It is applied as `build_incremental_timeline`'s and
  `build_fresh_timeline`'s `upto`, combined (`min`) with the requester's own departure for a
  left room, on incremental and initial syncs alike.
- `build_incremental_timeline` cuts its forward page at the newest event at or before `upto`.
  `RoomActor::paginate` returns events without their positions and `timeline_position` is a
  linear walk, so the boundary event is fetched with one backward page of one from `upto + 1`
  (a keyed read) and found in the forward page by id; not in the page and the page full means
  the bound is beyond the scan (a gap, answered by `build_fresh_timeline` from `upto`, which
  already honored it); not in the page and the page not full means nothing between the resume
  point and the bound is new. A bound at or before the resume point returns an empty timeline
  before any read.
- A hot room (`MembershipRecord::hot_room`) has no feed entries to bound it by -- and a stale
  one from before it went hot would hide everything since -- so it is read live, as before.
  Hot rooms remain fan-out-on-read and may repeat, as `resume_mode` has always documented.
  Found on the way (`hub.rs`): the hub rewrote a member's record, `hot_room` included, only
  when *that member's* membership changed or the record was missing, so when a room crossed
  the threshold every member already there kept a record saying "cold" -- no feed entries any
  more, and not a candidate without them, so nothing from that room reached them again. With
  the bound trusting the flag, that would have become a stale bound rather than a missing
  candidate; either way wrong. `apply_room_update` now rewrites a record whose `hot_room`
  disagrees with the room's current hot-ness, keeping its baseline position
  (`hub::tests::a_room_going_hot_is_written_to_the_records_of_the_members_already_there`,
  fails without it -- checked). The existing hub test did not see this because it started
  watching the room after its creation, so the creator's record was "missing" at the flip.
- A `tracing::debug!` per batch, `"assembled a /sync batch"`, with `user_id`, `since`, `next`,
  `initial`, `rooms` and `timeline_events`. No metric: `hs-user` registers none today and a
  batch counter is not worth a registry hook.

**What it does not change.** No latency on the common path: one keyed read per room in a
batch (the pointer) and one keyed read per room with a non-empty timeline (the boundary),
both O(log n) on the in-memory backend; the feed walk only when the room has moved past the
token. `wait_for_consumed` and `settle_before_read` are untouched. To-device, device lists,
account data, typing, receipts and presence keep their own cursors and were never affected.
A requester with no device (some appservice callers) still records no cursor, so its entries
still coalesce; that row in "Known gaps" stands.

**Tests** (`cargo test -p hs-user`: 140 lib + 6 scenario, from 136 + 6):

- `sync::tests::an_event_that_arrives_during_assembly_is_in_exactly_one_batch`: a writer task
  sends 300 messages as fast as it can (yielding between sends) while a device syncs in a loop
  the way `routes::sync` does (build, then record the cursor), initial batch included, on a
  four-thread runtime. Asserts no batch is `limited`, no event id is sent twice, none is lost,
  and the batches concatenated are the room's order. On `main` before the fix: `159 of 300
  events were sent twice across 136 batches`. After: passes, five runs in a row, 0.3 s each.
- `sync::tests::an_incremental_timeline_ends_at_the_tokens_position_not_the_rooms_live_end`:
  `build_incremental_timeline` directly, six messages: unbounded, bounded mid-stretch, bound
  at and before the resume point (empty), bound beyond the room's end, a gap answered with the
  newest `limit` events at or before the bound (`limited: true`), and exactly `limit` events
  up to the bound with more beyond it (not a gap).
- `store::tables::tests::room_pos_at_token_is_the_rooms_newest_entry_unless_that_is_past_the_token`:
  the pointer path, coalescing, pinning, and the fallback walk.
- `crates/hs-cli/tests/bridge_offerings.rs`: its `Watch` client no longer de-duplicates; it
  remembers every event id in every batch it is sent (the initial one and the ones passed
  over included) and fails on a repeat. The dedupe existed only for this bug. Against the real
  binary with the bound taken back out (a one-line mutant passing `departed_at` instead of
  `upto`), `a_person_gets_a_bridge_by_messaging_its_front_door_and_the_manager_bot_takes_commands`
  fails at once: `$B933_... in !rf9E...:example.org was sent in an earlier batch and again in
  this one`. With the fix, 3 of 3 pass (33 s).

**Verification**: `cargo fmt --all --check`; `cargo clippy -p hs-user --all-targets -- -D
warnings`; `cargo test -p hs-user`; `cargo test -p hs-cli --test bridge_offerings`; results
in the section's commit and in `docs/next-steps.md`'s struck row.

## Where this stopped (2026-09-27, branch `agent/federation-edus`): ephemeral data across servers and restarts

**Done and verified by running** (all on this branch):

- Receipts and presence are durable. `hs_user::store::UserStore` gained `put_receipt`/`list_room_receipts`
  and `put_presence`/`get_presence` (keyspaces `hs_user.receipts`, `hs_user.presence`);
  `ReceiptRegistry`/`PresenceRegistry` write through and load lazily per room/user. The typing, receipt
  and presence counters are `hs_user::stamp::Stamps` (max(previous+1, unix micros)), so a token from
  before a restart neither hides new data nor re-shows old. Tests:
  `sync::tests::receipts_and_presence_are_in_sync_after_a_new_hub_over_the_same_store` (fails with the
  registries built without the store -- checked), `receipts::tests::receipts_outlive_the_registry_that_recorded_them`,
  `presence::tests::presence_outlives_the_registry_that_recorded_it`, `stamp::tests::*`.
- EDUs both ways: `hs_user::edu` (EduOutbox seam, InboundEdu parsing with origin checks),
  `SessionHub::install_edu_outbox`/`receive_edu` (typing incl. a stop EDU when a typing lapses, m.read
  receipts only, presence on set and on a /sync-driven change). `hs_federation::sender::FederationSender::enqueue_edu`
  (in-memory per destination, 100 per transaction, coalescing keys, gate-respecting),
  `hs_federation::edu::InboundEduSink` + `FederationState::edu_sink`, `/user/keys/query` and
  `/user/keys/claim` real (`transport/keys.rs`), `/user/devices` answers even with device-name lookup off
  (names stripped). `hs_e2e::federation`: RemoteKeys hook so local /keys/query and /keys/claim ask a remote
  user's server (no cache), plus `federation_keys_query`/`federation_keys_claim`. `hs_cli::edus`:
  SenderEduOutbox, EduDispatcher (typing/receipts/presence to the hub, m.device_list_update and
  m.signing_key_update to the device-list stream), DeviceListAnnouncer (polls the e2e stream every 200 ms and
  sends m.device_list_update per device to servers sharing a room), ClientRemoteKeys; wired in `serve.rs`.
- `crates/hs-cli/tests/federation_edus.rs` (two in-process servers): typing, stop-typing, read receipts and
  presence cross A->B and B->A; a device added on B (login + key upload) is in alice's `device_lists.changed`
  on A and A's /keys/query returns it from B, and the reverse. Both pass (76 s, debug build); both fail with
  the inbound EDU sink not installed (checked).
- Commands run green: `cargo fmt --all --check`; `cargo clippy -p hs-user -p hs-federation -p hs-e2e
  --all-targets -- -D warnings`; `cargo test -p hs-user` (136+6), `-p hs-federation` (158), `-p hs-e2e`
  (26 lib + 3 new `tests/remote_keys.rs` + existing), `cargo test -p hs-cli --test federation_edus` (2/2).

**Not run / not done:**

- `cargo clippy -p hs-cli --all-targets -- -D warnings` and the rest of `cargo test -p hs-cli` (e2e,
  federation_two_servers, federation_writes, federation_reads, federation_restart) were not run after the
  hs-cli wiring; `cargo check -p hs-cli --tests` passes. Run them first.
- No real-binary restart test for receipts/presence yet (the in-process server cannot be restarted over its
  data dir); the durability proof is the hub-over-the-same-store unit test. Next: a test in the style of
  `crates/hs-cli/tests/federation_restart.rs` that sets a receipt and presence, SIGTERMs `hs serve`, restarts it
  over the same data dir, and checks an initial /sync.
- To-device over federation (m.direct_to_device) is not sent or received. Cross-signing changes go out as
  m.device_list_update, not m.signing_key_update. Device-list changes made while the server was down are not
  announced. EDUs are dropped (not stored) for destinations another cluster replica sends for, so in cluster
  mode a user's typing/receipts/presence reach only destinations their replica owns. Presence is not pushed to
  a server when it newly shares a room. Complement's TestDeviceListUpdates remote halves were not run (laptop).

## Session 7 (2026-09-27): `/sync` is cluster-aware

**Task**: `docs/next-steps.md` item 1, the bullet that matters most to a client: the session hub
watched only its own replica's room stream, so a long-poll on replica B for a room replica A owns
was never woken by A's events, and a client's write through A was not necessarily in its next
`/sync` on B. Scope: `crates/hs-user`, additive mesh methods in `crates/hs-cluster`, one new
module and a few lines of wiring in `crates/hs-cli`, `docs/scaling.md`, RFC 0018. Track 03 is
concurrently changing `serve.rs`'s advertise address, `/createRoom`'s gating and the mesh's TLS
wiring; nothing here touches any of those.

### The design, in five lines

1. **A session lives wherever its `/sync` arrived.** No user shard is consulted and no `/sync`
   is forwarded: every replica reads the same feeds, memberships and device cursors from the
   shared store, so any replica can answer any user. `PLAN.md` 5.4's user-session *owner* is
   not built; what is built is the part of it a client can tell apart -- the wake and the
   read-your-writes -- without a second routing layer.
2. **Only a room's owner writes feeds; it then rings every other replica's doorbell.** The
   owner's hub processes the registry stream as before (feed entries, membership rows) and
   hands each processed update to `SessionCluster::publish` as a `RoomWake` (room, position,
   the owner's stream number, the users it woke). A per-peer pump coalesces wakes into one
   `user.wake` batch per mesh round trip to *every live replica* (the shard map's distinct
   owners); the receiving hub wakes those users' long-polls. Broadcast, not registration:
   O(replicas) small messages per event and no per-room interest table to keep consistent
   across failovers. A non-owner's hub writes nothing for rooms it does not own.
3. **A non-owner reads a room through a mirror validated against the store, never through its
   registry.** `SessionHub::room` sends owned rooms to the registry and every other room to
   `RoomMirror`: a read-only `RoomActor::load` snapshot per room, reloaded whenever the store's
   durable timeline head (`room_sn` lookup plus a reverse range of one on `room_timeline`) is
   past the snapshot's. The store is the source of truth; the wake is only the doorbell, the
   rule the appservice pump already follows.
4. **Read-your-writes across replicas is `wait_for_consumed` with the peers' numbers.** Before
   reading, `/sync` on B asks every live peer what it has published (`user.positions`, one small
   mesh round trip per peer, concurrently, 250 ms cap) and waits, within the same 500 ms budget
   the single-replica wait already had, until B has received each peer's wakes up to that
   number (every batch carries the sender's consumed high-water mark, keyed by
   `replica#generation` so a restarted peer starts over). A's wake is sent only after A's hub
   wrote the feed entries, so when B has it the entries are durable and the mirror check in (3)
   sees the new head.
5. **What it does not do.** Typing, receipts and presence stay in memory per replica and are not
   exchanged. To-device and device-list changes still rely on the long-poll's 500 ms re-check.
   A mirror reload is a full `RoomActor::load`, O(room size) per new event in a room this
   replica does not own but has sessions in (RFC 0018 asks `hs-room` for an incremental
   catch-up). A single replica in `mode=cluster` pays nothing (no peers); single-node mode is
   untouched (no cluster is installed on the hub, and every code path is the old one).

One thing the task description overstated, found while proving the test fails without the wake:
the long-poll already re-checks its durable feed every 500 ms (`E2E_POLL_INTERVAL`, there for
to-device traffic), so with a shared store a cross-replica long-poll was never held for the full
timeout -- it was held for up to half a second and then answered *from a stale registry copy of
the room*. The wake takes the half second to the owner's feed latency; the mirror is what makes
the answer right. The unit test's bar is therefore "faster than the re-check" (400 ms; in-process
it is single-digit milliseconds), and the mutant without the wake fails at 507 ms.

### Verified by running: two `hs serve` processes on one PostgreSQL 16

Setup: PostgreSQL 16 from apt on `127.0.0.1:5432` (`service postgresql start`; role `hs`,
database `hs05`), two configs differing only in the client port (18140/18141) and mesh port
(18549/18550), `cluster.single_node: false`, `room_shards: 4`, `user_shards: 4`, shared secret
mesh auth (plain TCP; TLS is track 03's), one signing-key directory shared by both, a debug
build of `hs` from this branch, `RUST_LOG=info,hs_user::cluster=debug,hs_user::hub=debug,hs_cli::sync_cluster=debug`.
The machine: 4 cores shared with three other agents' cargo builds throughout.

```
$ hs serve -c a.yaml &   # A: client 18140, mesh 18549
$ hs serve -c b.yaml &   # B: client 18141, mesh 18550
A ready: 200
B ready: 200
a.log: INFO hs_cli::sync_cluster: /sync is cluster-aware: room owners wake this replica's long-polls over the mesh replica=127.0.0.1:18549#1790482095157
b.log: INFO hs_cli::sync_cluster: /sync is cluster-aware: room owners wake this replica's long-polls over the mesh replica=127.0.0.1:18550#1790482095160
```

`run.py` (in the session's scratch directory; it registers alice and bob on A, creates four
rooms on A, bob joins each through the gated `/rooms/{id}/join`, then measures). All four rooms
hashed to shards A owns (A's log has no mirror loads, B's has 92), so "poll on B, send on A" is
the direction that crosses the mesh and "poll on A, send on B" is a send forwarded to the owner
with a local wake:

```
== wake latency: alice long-polls on X (timeout 30 s), bob sends on Y ==
  room poll on send on  ack->return  start->return  event in poll
     0       B       A     324.0 ms       409.6 ms           True
     0       A       B     273.7 ms       358.8 ms           True
     1       B       A     366.6 ms       442.9 ms           True
     1       A       B     258.6 ms       336.2 ms           True
     2       B       A     368.1 ms       468.5 ms           True
     2       A       B     254.4 ms       331.3 ms           True
     3       B       A     390.0 ms       508.0 ms           True
     3       A       B     261.2 ms       353.4 ms           True

== read-your-writes: alice sends on X, immediately syncs on Y with timeout=0 ==
room 0: send on A, sync on B: 20/20 seen, 0 missed, slowest sync 413.3 ms
room 1: send on A, sync on B: 20/20 seen, 0 missed, slowest sync 361.2 ms
room 2: send on A, sync on B: 20/20 seen, 0 missed, slowest sync 405.3 ms
room 3: send on A, sync on B: 20/20 seen, 0 missed, slowest sync 623.1 ms
room 0: send on B, sync on A: 20/20 seen, 0 missed, slowest sync 313.6 ms
room 1: send on B, sync on A: 20/20 seen, 0 missed, slowest sync 618.4 ms
room 2: send on B, sync on A: 20/20 seen, 0 missed, slowest sync 363.8 ms
room 3: send on B, sync on A: 20/20 seen, 0 missed, slowest sync 336.2 ms
```

160 of 160 writes through one replica were in the very next `timeout=0` sync on the other, in
both directions. The long-poll returned in 250-390 ms after the send was acknowledged, never
the timeout -- but "tens of milliseconds" it is not, and the *local* direction is nearly as slow,
so the time is not the mesh. `run2.py` puts wall-clock stamps next to the replica logs (same
machine, same clock) for a fresh room, and first measures what a `/sync` costs when nothing is
new:

```
baseline: alice's timeout=0 sync on B with nothing new: 75, 75, 75, 83, 75 ms
baseline: alice's timeout=0 sync on A with nothing new: 92, 75, 78, 72, 82 ms

round 0 poll on B, send on A: send started 04:12:18,703, acked 04:12:18,781 (79 ms), poll returned 04:12:19,088: ack->return 306 ms
round 0 poll on A, send on B: send started 04:12:19,662, acked 04:12:19,748 (86 ms), poll returned 04:12:19,988: ack->return 240 ms
round 1 poll on B, send on A: send started 04:12:20,649, acked 04:12:20,730 (81 ms), poll returned 04:12:21,037: ack->return 307 ms
round 1 poll on A, send on B: send started 04:12:21,656, acked 04:12:21,737 (82 ms), poll returned 04:12:21,975: ack->return 238 ms
round 2 poll on B, send on A: send started 04:12:22,714, acked 04:12:22,792 (78 ms), poll returned 04:12:23,095: ack->return 303 ms
round 2 poll on A, send on B: send started 04:12:23,795, acked 04:12:23,883 (89 ms), poll returned 04:12:24,145: ack->return 262 ms

a.log 04:12:18,886 DEBUG hs_cli::sync_cluster: sent a wake batch to a peer peer=127.0.0.1:18550 consumed=180 rooms=1
b.log 04:12:18,886 DEBUG hs_user::hub: received a wake batch from a peer from=127.0.0.1:18549#... consumed=180 rooms=1 users=2
b.log 04:12:18,932 DEBUG request{...}: hs_user::cluster: room mirror loaded a room this replica does not own room_id=!WKWFowBu5upLSA9KJz:cluster.local head=11
```

Round 0, cross-replica, read against the logs: the send was acknowledged at 18,781; A's hub fed
the update and B had the wake at 18,886 (105 ms, which is A's feed writes over PostgreSQL in a
debug build -- a long-poll *on A* waits for the same thing); the mesh hop is inside one
millisecond (A logs "sent" after B's reply, B logs "received" before answering, same
millisecond); B's mirror had reloaded the room at 18,932 (46 ms); the response was on the wire
at 19,088 (156 ms to build, against 75 ms for a response with nothing in it). So of the 306 ms,
under 1 ms is the mesh, about 60 ms is the cross-replica premium (the mirror reload, RFC 0018),
and the rest is what this debug binary pays for any `/sync` with news over PostgreSQL. A
release build is the honest way to get the absolute number; see the next paragraph for what it
gave, or that it did not finish.

The same two scripts against a **release** build (same host, same PostgreSQL, the other agents'
builds still running):

```
baseline: alice's timeout=0 sync on B with nothing new: 38, 41, 41, 40, 42 ms
baseline: alice's timeout=0 sync on A with nothing new: 44, 38, 44, 37, 37 ms

round 0 poll on B, send on A: send started 04:25:03,599, acked 04:25:03,643 (44 ms), poll returned 04:25:03,794: ack->return 151 ms
round 0 poll on A, send on B: send started 04:25:04,236, acked 04:25:04,277 (40 ms), poll returned 04:25:04,396: ack->return 119 ms
round 1 poll on B, send on A: send started 04:25:04,876, acked 04:25:04,915 (39 ms), poll returned 04:25:05,052: ack->return 137 ms
round 1 poll on A, send on B: send started 04:25:05,509, acked 04:25:05,552 (43 ms), poll returned 04:25:05,722: ack->return 171 ms
round 2 poll on B, send on A: send started 04:25:06,257, acked 04:25:06,301 (44 ms), poll returned 04:25:06,477: ack->return 176 ms
round 2 poll on A, send on B: send started 04:25:07,009, acked 04:25:07,067 (58 ms), poll returned 04:25:07,243: ack->return 176 ms

a.log 04:25:03,697 sent a wake batch to a peer consumed=5 rooms=1
b.log 04:25:03,696 received a wake batch from a peer consumed=5 rooms=1 users=2
b.log 04:25:03,721 room mirror loaded a room this replica does not own head=9

== wake latency (4 rooms, both directions): ack->return 168-201 ms, event in poll: 8/8 ==
== read-your-writes: 160/160 seen, 0 missed; slowest sync 173-219 ms in 14 of 16 cells,
   516.7 ms and 1418.3 ms in the other two (single outliers; the 500 ms bounded wait plus a
   stalled response build on a box running three cargo builds, not explained further) ==
```

Round 0 cross-replica, release: acked at 03,643; B had the wake at 03,696 (53 ms: A's hub
feeding the update over PostgreSQL); the mirror had reloaded at 03,721 (25 ms); the response
was on the wire at 03,794 (73 ms to build, against 40 ms for an empty one). So a cross-replica
long-poll returns about 150 ms after the write is acknowledged, of which the mesh is under a
millisecond and the cross-replica premium (the mirror reload) about 25 ms; a same-replica
long-poll is 120-170 ms on the same box. The wake is not the bottleneck; the feed write and the
response build are, and both are single-node work.

**Found on the way, for the config owner (track 03/hs-config): per-replica settings live in the
shared store.** Settings are stored in the database and the bootstrap file only seeds it on the
first run (`crates/hs-cli/src/bootstrap.rs`, `hs_config::layered`: database outranks file). Two
replicas seeding one database race, and the loser's `listeners` and `cluster.mesh.port` then
apply to *both* on the next restart: on my second start replica A came up as B (bound 18141 and
mesh 18550, "Address already in use", exit 1). `HS__` overrides cannot fix the listener (they
set scalars, not list entries). Workaround used:
`hs config -c a.yaml unset /listeners/listeners` and `unset /cluster/mesh/port`, after which
each replica's own file supplies them and the store does not re-seed. A cluster deployment
needs either those sections excluded from seeding in `mode=cluster`, or documentation saying
so; nothing in this branch changes it.

Not verified by running: two *pods*; a peer that dies mid-run (the unit test covers a peer whose
wakes never arrive: the sync is delayed by the bounded wait and still sees the write, from the
store); more than two replicas; any room with more than a handful of events (the reload cost is
reasoned about, not measured).

### Automated tests

- `crates/hs-user/src/cluster.rs`, `two_replica_tests`: two hubs, two registries, one shared
  `MemoryBackend`, replica A owning every room and B none, a `FakeCluster` carrying A's wakes to
  B in-process (with a configurable delay, and a mute switch). `a_long_poll_on_the_other_replica_is_woken_by_the_owners_write`
  fails without the wake (mutant checked by hand: 507 ms, the re-check interval, against the
  400 ms bar); `a_write_through_the_owner_is_in_the_very_next_sync_on_the_other_replica` sends
  ten times through A and syncs `timeout=0` on B after each with wakes delayed 150 ms, which
  without the peer wait would read the feeds before A's hub wrote them;
  `a_peer_whose_wakes_never_arrive_delays_a_sync_only_by_the_bounded_wait`;
  `a_hub_that_does_not_own_a_room_feeds_nobody_for_it`;
  `the_mirror_reloads_a_room_only_when_the_store_is_ahead_of_its_snapshot`. Plus `WakeBatch`
  coalescing and JSON round-trip.
- `crates/hs-cluster/tests/mesh_peer.rs`: `POST /mesh/v1/peer` end to end over a loopback
  socket with shared-secret auth: the handler sees the sender's identity, route and payload;
  later messages reuse the pooled connection; a port nobody listens on is `PeerUnreachable`
  promptly; a server without a peer handler is reported unreachable (its `501`), not answered;
  the wrong secret is refused before the handler sees anything.
- `crates/hs-cli/src/sync_cluster.rs`: the peer handler's `user.positions` answer, a
  `user.wake` batch advancing the hub's per-peer mark, junk and unknown routes refused; a
  single-node replica owns every room and has no peers.
- Every pre-existing `hs-user` test still passes (123 lib, 6 scenario); `hs-cluster` and
  `hs-cli` suites as recorded under "How to verify".

### Files

- `crates/hs-user/src/cluster.rs` (new): `RoomWake`, `WakeBatch`, `PeerPosition`,
  `SessionCluster`, `RoomMirror`, the in-process `FakeCluster` for tests.
- `crates/hs-user/src/hub.rs`: `install_cluster`, `owns_room`, `room` (the one way this crate
  now reads a room), `receive_wakes`, `peer_consumed`, `settle_before_read`; the drain loop
  publishes a wake after every update; `process_room_update` skips rooms this replica does not
  own and reports whom it woke.
- `crates/hs-user/src/sync/mod.rs`: `build` calls `settle_before_read`; the three room reads
  go through `hub.room`.
- `crates/hs-cluster/src/mesh/envelope.rs`: `PeerHandler`. `mesh/server.rs`: `MeshDeps.peers`,
  `POST /mesh/v1/peer`. `mesh/forwarder.rs`: `Forwarder::send_to_peer`. `error.rs`:
  `ForwardError::PeerUnreachable`. `ownership.rs`: `ShardMap::replicas`.
- `crates/hs-cli/src/sync_cluster.rs` (new): `MeshSessionCluster`, `SessionPeerHandler`,
  `install`. `cluster.rs`: `ClusterHandles::{origin, origin_generation, install_peer_handler}`
  and the `peers` field of `MeshDeps` in `spawn_mesh`. `serve.rs`: one `install` call after the
  cluster starts and one `identity.clone()` for the mirror. `lib.rs`: the module.
- `docs/scaling.md`: the "clients connected" row is fact; two new "does not add" rows; the
  honest-status bullet and the list of what would turn design into fact.
- `docs/rfcs/0018-room-actor-catch-up.md`: the incremental catch-up `hs-room` needs.

### How to verify

```
cargo fmt --all --check
cargo clippy -p hs-user -p hs-cluster -p hs-cli --all-targets -- -D warnings
cargo test -p hs-user -p hs-cluster
cargo test -p hs-cli
```

For the two-process run: `service postgresql start`, a role and database
(`psql -h 127.0.0.1 -U postgres -c "CREATE USER hs PASSWORD 'hs'" -c "CREATE DATABASE hs05 OWNER hs"`
works with the apt package's `trust` rule for `127.0.0.1`), the two configs above, one signing
key directory, `hs serve -c a.yaml & hs serve -c b.yaml &`, then `run.py` and `run2.py`
(kept in the session scratch directory, not the repository; both are twenty lines of
`requests` calls and are reproduced in spirit by `crates/hs-user/src/cluster.rs`'s
`two_replica_tests`).

### Decisions made

- **Broadcast, not registration.** A room owner sends its wake to every live replica rather than
  keeping a table of which replicas hold sessions for which rooms. O(replicas) tiny messages per
  event, coalesced per peer; no state to lose on failover. Revisit if replica counts get large.
- **Ask the peers, not the client.** Read-your-writes across replicas costs one mesh round trip
  per peer per `/sync` (`user.positions`, concurrently, sub-millisecond on a host). The
  alternative -- carrying the owner's position back through the forwarded write's reply -- only
  covers a write that went through the same replica the sync then hits, which a load balancer
  does not promise. Piggybacking can be added as an optimisation if the loadgen slope says the
  round trip matters.
- **The store is the truth, the wake is the doorbell.** A non-owner's mirror is checked against
  the durable head on every access (two point reads), so a lost or late wake can delay a
  response but never make it wrong. This is what made the "peer whose wakes never arrive" case
  a bounded-latency case rather than a correctness case.
- **Only the owner feeds.** `process_room_update` writes nothing for a room this replica does
  not own, so two hubs never race on one user's feed from two views of a room.
- **Keyed by `replica#generation`.** A restarted peer's numbering starts over; its old mark must
  not satisfy a new wait.
- **`hs-user` does not depend on `hs-cluster`.** The `SessionCluster` trait is this crate's own;
  `hs-cli` implements it over the mesh. Tests stand two hubs up without a mesh.
- **A shared `target/` across worktrees clobbers same-named artifacts.** Cargo's metadata hash
  does not include a workspace member's path, so my worktree's `libhs_cluster-<hash>.rmeta` and
  the main checkout's are the same file; a build in one tree refreshes the fingerprint the other
  trusts. Symptom: `hs-cli` failed to see `PeerHandler` that `hs-cluster` had just compiled.
  Workaround used here: `touch` the crates' sources before every build, and `CARGO_INCREMENTAL=0`
  so a rebuild does not need a new 900 MB incremental session directory on a disk that was full
  (nothing under `target/` was deleted). The integration lead should know this when merging
  concurrent branches that touch one crate.

### Interfaces provided

- `hs_user::cluster::{SessionCluster, RoomWake, WakeBatch, PeerPosition, RoomMirror}`;
  `SessionHub::{install_cluster, owns_room, room, receive_wakes, settle_before_read, peer_consumed}`.
- `hs_cluster::mesh::{PeerHandler, Forwarder::send_to_peer}`, `MeshDeps::peers`,
  `POST /mesh/v1/peer`, `ShardMap::replicas`, `ForwardError::PeerUnreachable`.
- `hs_cli::sync_cluster::{install, MeshSessionCluster, SessionPeerHandler, WAKE_ROUTE, POSITIONS_ROUTE}`;
  `ClusterHandles::{origin, origin_generation, install_peer_handler}`.

### Interfaces needed

- From track 04: `RoomActor::catch_up` (RFC 0018), so a non-owner's mirror stops reloading
  whole rooms.
- From track 03 / the config owner: per-replica settings (`listeners`, `cluster.mesh.port`, the
  advertise address) must not be seeded into the shared config store, or a restarted replica
  takes on another's identity (details above under "Found on the way").
- From track 03: `/join/{roomIdOrAlias}` (and `/createRoom`, already on its list) shard-gated.
  Found while running: bob's `POST /join/{roomId}` on A for a room B owns was refused `503` by
  A's fence -- the fence doing its job -- because that path has no `/rooms/` segment for the
  gate to see. `POST /rooms/{roomId}/join` is gated and forwarded, and is what the scripts use.

### Shared dependencies added

None. `hs-user` gained no dependency; `hs-cli` uses `tokio::task::JoinSet` rather than adding
`futures`.


> **Integration note, 2026-09-19 (integration lead):** this file reports the encrypted loadgen
> scenario failing at step 11 with an "ATOMICITY VIOLATION" in `/keys/claim`, attributed to track
> 01's concurrent `hs-kv` work. **That diagnosis was wrong and there is no regression.** The
> repeated key id was the device's *fallback* key, which the spec allows to be handed out
> repeatedly — `crates/hs-e2e/src/routes/keys_claim.rs` falls back to `claim_fallback_key` once
> the one-time-key pool is exhausted. The probe fired `pool + 3` concurrent claims and asserted
> the three excess ones would come back empty, which was true only while `GET /sync` omitted
> `device_unused_fallback_key_types`: without that field `matrix-sdk` never uploaded a fallback
> key at all. Making sync correct — the very work in this session — is what made a fallback key
> exist to be served. The probe now counts fallback claims separately
> (`crates/hs-loadgen/src/scenario_encrypted.rs`) and the scenario passes: **50 distinct one-time
> keys claimed by 53 concurrent callers with no double-claim, the 3 excess correctly getting the
> reusable fallback key.** The atomicity guarantee is intact.

# 05. Sync: status

Track brief: `docs/workstreams/05-sync.md`. Owner crates: `hs-user` (this session's assignment
also covers `crates/hs-loadgen`, the real-client scenario `docs/next-steps.md` item 2 calls "the
single best test of whether this is a homeserver").

Last updated: 2026-09-19 (session 6: filters actually honour event-type/sender includes and
excludes, not just `limit`/`lazy_load_members`; `m.receipt`/`m.read.private`/`m.fully_read` exist
end to end; a stale-server false alarm from track 16's Element-Web session is corrected. Sessions
1-5 preserved unchanged further down.)

## Session 6 (2026-09-19): getting ahead of Element Web -- filter content matching, receipts

**Task**: another agent was pointing Element Web at this server for the first time. In priority
order: (1) check whether `room.timeline.types`/`not_types`/`senders`/`not_senders` (and the
`room.state` equivalents) were honoured, not just parsed -- they were not, and this crate's own
`filter.rs` said so in its own doc comment; (2) build `m.receipt`/`m.fully_read`, reserved and
unused since session 1; (3) check `docs/status/16-management-web-interface.md` partway through
for anything the Element-Web session found that belongs to this track. Scope:
`crates/hs-user/**`, `crates/hs-loadgen/**` and this file.

### 1. Filters: event-type/sender content matching, for both `room.timeline` and `room.state`

Before this session, `crate::filter::SyncFilter` parsed `types`/`not_types`/`senders`/
`not_senders` on both `room.timeline` and `room.state` but never applied them -- `crate::sync`
only ever looked at `limit`, `lazy_load_members`, `include_leave` and the room allow/denylist.
Element sends a filter (with `lazy_load_members: true`) on every sync, and any client whose
filter also restricts event types would have silently gotten everything back. Fixed:

- **`crates/hs-user/src/filter.rs`**: `RoomEventFilter::matches(event_type, sender) -> bool`
  (`not_types`/`not_senders` win outright over `types`/`senders`, matching
  `SyncFilter::room_allowed`'s existing denylist-wins precedent; `*` suffix wildcards supported,
  e.g. `"m.room.*"`) and `RoomEventFilter::is_content_noop()`. `SyncFilter::timeline_content_filter()`/
  `state_content_filter()` return `Option<&RoomEventFilter>`, `None` whenever that section is
  absent or sets no content restriction, so `crate::sync` can skip the filtered code path
  entirely in the overwhelmingly common case.
- **`crates/hs-user/src/sync/mod.rs`**: `build_incremental_timeline`/`build_fresh_timeline` both
  gained a `content_filter: Option<&RoomEventFilter>` parameter. When absent, behavior is
  byte-for-byte unchanged from before this session (same single `paginate` call at exactly
  `limit`, same peek-one-more `limited` check) -- zero regression risk for the overwhelmingly
  common unfiltered case. When present: one bounded `paginate` call at
  `limit.max(FILTERED_TIMELINE_SCAN)` (500), filter the raw batch, truncate to `limit`.
  **`limited` is conservative**: true whenever this response did not *prove* the room has nothing
  more for this window (either the filtered set already filled `limit`, or the raw scan itself
  was cut off by the 500-event cap before reaching the true end of history) -- worst case a
  client pages once more than strictly necessary and gets a smaller/empty page; this never skips
  a real event, the same direction `resume_mode`'s own fallback already errs in. Documented as a
  known simplification, not a silent gap: a filter that excludes nearly everything in a room with
  a very long tail (thousands of events since the filter's cutoff) will not scan past that cap in
  one response. `build_state_section` gained the equivalent filter as a plain, unbounded `Vec`
  filter (`full_state()` is already one-event-per-`(type, state_key)`, not a history, so no scan
  bound is needed there).
- `crate::filter`'s module doc rewritten: `room.timeline`/`room.state`'s `types`/`not_types`/
  `senders`/`not_senders` moved from "parsed but ignored" to "applied". Room-scoped account data
  and ephemeral events (`m.typing`, `m.receipt`) are **not** content-filtered even though the
  spec's `RoomEventFilter` shape technically allows it there too -- both are already small,
  bounded, non-history snapshots, so this was judged not worth building this session; recorded as
  a documented scope cut, not silently dropped.
- 10 new tests: `filter::tests` (wildcard matching, `not_types`/`not_senders` precedence,
  `is_content_noop`, the two `SyncFilter` accessor methods); `sync::tests::
  initial_sync_timeline_type_filter_excludes_non_matching_events` (exercises
  `build_fresh_timeline`'s filtered branch), `incremental_sync_timeline_type_filter_excludes_non_matching_events`
  (exercises `build_incremental_timeline`'s filtered branch -- this one initially failed for a
  reason unrelated to the filter logic itself, see "A test-harness gotcha" below),
  `state_not_types_filter_excludes_a_matching_state_event`.

### 2. `m.receipt` (`m.read`/`m.read.private`) and `m.fully_read`

`SyncToken::receipts_seq` had been reserved since session 1. Built exactly to the
typing/presence pattern this crate's own status file already named as the template:

- **`crates/hs-user/src/receipts.rs`** (new): `ReceiptRegistry`, in-memory, keyed by room --
  mirrors `crate::typing::TypingRegistry`'s shape exactly (global monotonic counter stamped per
  room, exposed via `SyncToken::receipts_seq`). **Not persisted**, a deliberate cut matching
  `crate::presence::PresenceRegistry`'s identical precedent (a restart loses read state, same as
  it already loses typing and presence) -- moving this into `crate::store::UserStore` later is a
  mechanical follow-up, not a redesign. `ReceiptRegistry::content_for(room_id, viewer)` builds the
  spec's `{event_id: {receipt_type: {user_id: {ts}}}}` shape **scoped to the viewer**: every
  `m.read` receipt is public (visible to any member), but an `m.read.private` receipt is omitted
  entirely unless `viewer` is its own sender -- the entire point of the private variant.
- **`crates/hs-user/src/hub.rs`**: `SessionHub::set_receipt`/`receipts_seq`/`receipt_content_for`,
  same wake-eagerly shape as `set_typing` (looks up joined members, calls the hub's `Notify`
  waker directly for each -- not privacy-scoped at wake time, only at content-build time, same
  "over-broad wake just costs a harmless extra pass" reasoning already used for typing/presence).
- **`crates/hs-user/src/routes/receipts.rs`** (new): `POST /rooms/{roomId}/receipt/{receiptType}/{eventId}`
  and `POST /rooms/{roomId}/read_markers`, both requiring current `join` membership (same rule
  `typing.rs` already uses). **`receiptType` accepts `m.fully_read` too, not just `m.read`/
  `m.read.private`** -- this matches Synapse's own accepted behavior on this same endpoint (ruma's
  own `create_receipt::v3::ReceiptType` models `FullyRead` as a real variant, and
  `matrix-rust-sdk`'s `Room::send_single_receipt` can be called with it directly), and is what
  makes a real client's obvious call actually work rather than 400ing on a legal spec extension.
  `m.fully_read`, from either endpoint, writes straight through the pre-existing
  `UserStore::put_room_account_data` -- it already has its own durable storage, its own change
  counter, and its own `/sync` wiring; no new plumbing needed for it at all.
- **`crates/hs-user/src/sync/mod.rs`**: gathered up front alongside typing (a receipt-only change
  never touches the feed either), folded into the same `ephemeral.events` array a room's `m.typing`
  event already occupies. `receipts_seq` is threaded into the outgoing token
  (previously present in the struct literal only via `..baseline`, now the one remaining field to
  set explicitly -- clippy's `needless_update` caught the resulting all-fields-explicit literal,
  fixed by dropping `..baseline` entirely).
- 16 new tests across `receipts::tests`, `routes::receipts::tests` and one `sync::tests` case
  (`a_receipt_wakes_a_long_poll_and_appears_as_m_receipt`, same shape as the existing typing wake
  test: a concurrent task sets a receipt 50ms into a 5s-timeout long poll, the poll returns in
  well under 2s).

### A test-harness gotcha (not a production bug)

`incremental_sync_timeline_type_filter_excludes_non_matching_events` initially failed with an
empty `rooms: {}` response -- not a filtering bug at all. `crate::store`'s feed **coalesces**
repeated updates to the same room into one still-unconsumed entry until some device's cursor
crosses it (`store::tables::tests::unconsumed_updates_to_the_same_room_coalesce` already proves
this). The existing passing test this one was modeled on
(`a_message_sent_after_a_token_was_issued_appears_in_the_next_incremental_sync`) calls
`hub.store().record_device_cursor(...)` right after the baseline sync for exactly this reason;
my first draft omitted that call, so the two new sends coalesced into the seed message's
already-synced feed entry instead of creating a fresh one past the baseline token. Fixed by
adding the same `record_device_cursor` call. Recorded here so the next person modeling a new
"seed, baseline, then more activity" test copies this detail too.

### Correction to track 16's Element-Web session: receipts do not 404 on a fresh server

`docs/status/16-management-web-interface.md`'s Element-Web session (read partway through this
one, per this session's own instructions) reported bug 3: `POST .../receipt/{receiptType}/{eventId}`
and `POST .../read_markers` both `404` live with an empty body, despite reading this exact crate's
`routes/receipts.rs` and confirming both handlers "implement... completely and correctly." **That
404 was real for the process they were curling, but it was not this session's code that was
missing -- it was a stale, already-running `hs serve` process.** That session's own write-up says
the server was started early (step 1) and "is still running... as this session ends" -- i.e. one
long-lived process, started before this session's `routes/mod.rs` mounting existed on disk (both
crates are edited concurrently in the same working tree; a running process never re-reads source
after it starts). Proof, against a server built and started *after* this session's routes landed:

```
$ hs generate-config --server-name verify.local -o config.yaml   # + enable_registration/shared secret, port patch
$ hs generate-signing-key -o signing-keys
$ hs serve -c config.yaml &
$ curl -X POST http://127.0.0.1:18099/_matrix/client/v3/register -d '{"username":"verifyuser","password":"verifypassword123","auth":{"type":"m.login.dummy"}}'
$ curl -X POST http://127.0.0.1:18099/_matrix/client/v3/createRoom -H "Authorization: Bearer $TOKEN" -d '{}'
$ curl -X PUT ".../rooms/$ROOM_ID/send/m.room.message/txn1" -H "Authorization: Bearer $TOKEN" -d '{"msgtype":"m.text","body":"hello"}'
$ curl -i -X POST ".../rooms/$ROOM_ID/receipt/m.read/$EVENT_ID" -H "Authorization: Bearer $TOKEN" -d '{}'
HTTP/1.1 200 OK
content-length: 2
{}
$ curl -i -X POST ".../rooms/$ROOM_ID/read_markers" -H "Authorization: Bearer $TOKEN" -d '{"m.fully_read": "$EVENT_ID"}'
HTTP/1.1 200 OK
content-length: 2
{}
```

Both `200`. Independently, this session's own `cargo test -p hs-loadgen --test real_client`
(below) drives the identical two routes through `matrix-rust-sdk`'s real HTTP client against a
binary built with `cargo build -p hs-cli --bin hs` in this same session, and both steps pass. **No
router-composition bug exists in `hs-http`'s `Builder`/`merge_router` for these paths** -- track
16's own diagnosis correctly ruled out several other causes but didn't have a way to know its
long-running server predated the routes it was reading about in source. Track 16's bugs 1 (no
CORS on `/_matrix/client/*`) and 2 (`GET /capabilities` reports stale `m.set_displayname`/
`m.set_avatar_url: false`) are both real and both squarely `hs-cli`'s (`serve.rs`'s router
assembly and `capabilities.rs` respectively) -- **not fixed here**, `hs-cli` was off limits this
session same as every prior one; flagged under "Interfaces needed" below since bug 1 in
particular blocks every browser client, Element included, from reaching any route this crate
owns.

### Verification

```
cargo fmt -p hs-user -p hs-loadgen                                    # clean
cargo test -p hs-user                                                 # 87 unit + 3 integration = 90 passed (was 70)
cargo build -p hs-cli --bin hs                                        # clean
cargo test -p hs-loadgen --test real_client -- --nocapture            # 28 steps, passed (below)
cargo test -p hs-loadgen --test real_client_encrypted -- --nocapture  # 16 steps, passed, still decrypts
```

**`cargo clippy -p hs-user -p hs-loadgen --all-targets -- -D warnings`: blocked by an unrelated,
persistent (not transient) failure in `crates/hs-http/src/cors.rs:85`** (`matrix_layer()` is
`#[must_use]` with no message, wrapping an already-`#[must_use]` `CorsLayer` -- clippy's
`double_must_use`), reproduced identically across four retries over roughly 20 minutes, unlike
the transient `hs-room`/`hs-e2e` compile errors from concurrent edits also hit this session (which
did resolve within a few retries each, same as prior sessions' notes describe). `hs-http` is not
this track's crate (nor is it in scope for this session), and clippy lints every local path
dependency, not just `-p` targets, so this is an environmental block, not a signal about
`hs-user`/`hs-loadgen`'s own code -- **confirmed by a clean `cargo clippy -p hs-user -p hs-loadgen`
run earlier in this same session, before this cross-track regression appeared** (right after the
filter/receipts code was written and two real clippy findings in it -- a `needless_update` and a
`bool_assert_comparison` in a test -- were fixed). Whoever owns `hs-http` next: `git diff` on
`crates/hs-http/src/cors.rs` should show a very recent, small change; either remove the redundant
`#[must_use]` on `matrix_layer()` or give it an explicit reason string.

**`real_client` run** (28 steps, up from 23 -- filters and receipts added as steps 15-16, and
`m.push_rules` now hard-succeeds where session 5 logged it as `KNOWN BUG`: the `hs-cli` wiring
that session asked for has since landed):

```
registered @loadgen-alice:hs-loadgen.test
registered @loadgen-bob:hs-loadgen.test
logged in @loadgen-alice:hs-loadgen.test on a second device via POST /login
alice created room !kGrlBQbuCGsjeFcBKZ:hs-loadgen.test
alice invited @loadgen-bob:hs-loadgen.test
@loadgen-bob:hs-loadgen.test joined !kGrlBQbuCGsjeFcBKZ:hs-loadgen.test
both clients completed a baseline /sync
alice's baseline /sync carried m.push_rules global account data
alice sent $aJTicMtFT2WrO4kMgtRHqxj5HYUbwo9QeI704NTy_mw ("hello bob, this is alice")
bob sent $5ronQm17rMByo4JbV9BAaIjdzdn4ngF97Bpbdy98qb4 ("hi alice, bob here")
bob's incremental /sync saw alice's message
alice's incremental /sync saw bob's message
alice's display name round-tripped through GET/PUT /profile
room name and topic changes appeared in /sync's timeline
room membership lists both @loadgen-alice:hs-loadgen.test and @loadgen-bob:hs-loadgen.test
backward /messages page (no `from`, the live end) contains alice's message (10 events)
backward /messages page, paginated from a token /sync handed back (not /messages itself), contains alice's message (10 events)
forward /messages page, paginated from a /sync token issued before any messages, contains alice's message (5 events)
bob's /sync saw alice's typing notice within the bounded wait
carol's /sync saw her invite to !kGrlBQbuCGsjeFcBKZ:hs-loadgen.test within the bounded wait
bob's /sync saw alice's profile change reflected in her m.room.member event
alice uploaded a sync filter capping room.timeline.limit to 1, filter id 3sTN4Hrqs0VBe0b4
the uploaded filter round-tripped through GET /user/{userId}/filter/{filterId}
a sync filter's room.timeline.limit was honoured: 1 event(s) returned, limited=true
alice's /sync saw bob's public read receipt on $aJTicMtFT2WrO4kMgtRHqxj5HYUbwo9QeI704NTy_mw
bob's /sync reported his own m.fully_read marker on $aJTicMtFT2WrO4kMgtRHqxj5HYUbwo9QeI704NTy_mw
both clients logged out
post-logout /sync was correctly rejected: the server returned an error: [401 / M_UNKNOWN_TOKEN] 401 Unauthorized M_UNKNOWN_TOKEN: Unrecognised access token
test matrix_rust_sdk_talks_to_a_real_hs_serve ... ok
```

**`real_client_encrypted` run**: unchanged shape, 16 steps, still decrypts end to end -- no
regression from this session's `sync/mod.rs`/`hub.rs` changes.

### Decisions made this session

- **Receipts are in-memory, not persisted** -- see `crate::receipts`'s module doc and "2." above.
  Matches the presence precedent exactly; a mechanical follow-up to move into `UserStore` later,
  not a redesign.
- **`m.fully_read` is accepted on `POST .../receipt/m.fully_read/{eventId}` as well as
  `.../read_markers`**, matching Synapse's own real behavior on that endpoint rather than the
  spec's narrower documented surface -- see "2." above for why (ruma models it, `matrix-rust-sdk`
  can call it directly).
- **Filtered timeline scanning is bounded at 500 raw events per response
  (`FILTERED_TIMELINE_SCAN`)**, with a conservative `limited` flag rather than looping until
  `limit` post-filter events are found -- see "1." above. Chosen over an unbounded loop to keep
  the "bounded response" guarantee `TO_DEVICE_LIMIT` already established elsewhere in this crate.
  `room.timeline`/`room.state` content filtering does not extend to `room.ephemeral`/
  `room.account_data` this session -- both are already small, non-history snapshots, judged not
  worth building given the session's actual priorities (Element sends filters far more for
  `lazy_load_members`/timeline shaping than for ephemeral-event type filtering).
- Track 16's Element-Web bug 3 (receipts 404) diagnosed as a stale-server artifact, not a real
  bug -- see the correction above. Bugs 1 (no CORS) and 2 (stale capabilities flags) are real,
  confirmed by reading, and belong to `hs-cli` -- forwarded, not fixed (out of scope this
  session).

### Interfaces provided (new this session)

- `POST /rooms/{roomId}/receipt/{receiptType}/{eventId}` (`m.read`, `m.read.private`,
  `m.fully_read`) and `POST /rooms/{roomId}/read_markers` (`crate::routes::receipts`).
- `SessionHub::set_receipt`/`receipts_seq`/`receipt_content_for` (`crate::hub`), `crate::receipts::
  ReceiptRegistry`/`ReceiptKind` -- all `pub`, usable by another crate that gets a `SessionHub`
  handle (none does today).
- `m.receipt` is a new addition to `/sync`'s existing `ephemeral.events` shape, not a new
  endpoint for another track to integrate against. `crate::filter::RoomEventFilter::matches`/
  `is_content_noop` and `SyncFilter::timeline_content_filter`/`state_content_filter` are new
  `pub` methods, usable by any future caller that wants this crate's own event-type-matching
  logic rather than reimplementing it.

### Interfaces needed

- **From `hs-cli`** (forwarded from track 16's Element-Web session, both confirmed real by
  reading the cited source): (1) a CORS layer on the `/_matrix/client/*` router, unconditional
  per the spec (distinct from the admin API's same-origin-by-default policy) -- currently CORS is
  applied only to the admin router in `serve.rs`; every browser client hosted on a different
  origin (Element included) cannot make a single client-server API call without this. (2)
  `crates/hs-cli/src/capabilities.rs`'s `get_capabilities()` still hardcodes `m.set_displayname`/
  `m.set_avatar_url` to `"enabled": false` from before `hs-room`'s profile routes existed; both
  routes work correctly today (confirmed live) and the capabilities response should say so.
- Nothing new required of any other track for this session's own work (filters and receipts are
  entirely self-contained within `hs-user`).

### What's next for track 05

1. Presence's idle/logout-driven automatic offline transition (deferred since session 4,
   unchanged scope cut).
2. Persist receipts into `crate::store::UserStore` instead of `crate::receipts::ReceiptRegistry`'s
   in-memory registry, once a session has time for the table/schema addition -- see "Decisions
   made" above.
3. `room.ephemeral`/`room.account_data` content filtering, if a real client is ever observed
   actually setting those filter fields (not observed yet; Element's own filter usage per track
   16's session was `lazy_load_members`, not ephemeral-type filtering).
4. Once `hs-cli`'s owner adds the CORS layer and fixes the stale capabilities flags (both
   forwarded above), re-verify Element Web's own browser run can proceed past both blockers.

## Session 5 (2026-09-19): the three things sync still owed -- push rules, `/keys/changes`, room summaries

**Task**: three gaps this crate's own status file and track 10's had already named as squarely
this track's to close, all with the other half already built: track 10 (`hs-push`) built
`account_data_for_sync`/`get_room_counts` and documented the exact recipe in
`docs/status/10-push.md`'s "Interfaces provided"; track 08 (`hs-e2e`) built the
`SyncTokenResolver` hook on `GET /keys/changes` and documented it in
`crates/hs-e2e/src/state.rs`; room summaries were this crate's own "What's next" item 1 from
session 4. Scope: `crates/hs-user/**`, `crates/hs-loadgen/**` and this file --
`crates/hs-e2e`, `crates/hs-push`, `crates/hs-room`, `crates/hs-auth` and `crates/hs-cli` were off
limits (another agent working across the first three; `hs-cli` held by another agent too).

### 1. `m.push_rules` and `unread_notifications`/`unread_thread_notifications`

Implemented exactly to track 10's documented recipe, with one structural choice made to avoid
touching `hs-cli` (off limits this session, see "hs-cli wiring needed" below):

- **`SyncToken` gained `push_rules_seq: u64`, wire version `2` -> `3`** (`crates/hs-user/src/token.rs`),
  following the file's own precedent for the `1`->`2` bump that added `typing_seq`: `PAYLOAD_LEN`
  grew from `1 + 7*8` to `1 + 8*8`, `encode`/`decode` extended, every test's struct literal updated
  (`initial_is_all_zero`, `json_round_trips_as_a_string`, the `round_trips_for_arbitrary_field_values`
  proptest, `decode_rejects_unsupported_version`'s zero-padding length). A version-3 token is the
  only kind this build now accepts, same "no long-lived client crosses a version bump" reasoning
  as the earlier bump.
- **`SessionHub` (`crates/hs-user/src/hub.rs`) gained two `OnceLock`-backed, idempotently-installable
  stores**, mirroring the existing `hs_e2e::state::E2eState::sync_token_resolver`/
  `hs_room::registry::RoomRegistry`'s `GlobalTokenResolver` convention exactly (install once, a
  second install is logged and ignored, absent-by-default means "behave exactly as before this
  session"):
  - `install_push_rules_store(Arc<hs_push::rulesets::CachedRulesetStore<hs_push::rulesets::tables::TablesRulesetStore<B>>>)` /
    `push_rules_store() -> Option<&Arc<...>>`
  - `install_counts_store(Arc<dyn hs_push::counts::CountsStore>)` / `counts_store() -> Option<&Arc<dyn CountsStore>>`
  - **Why a `OnceLock` method, not a `SessionHub::new` parameter** (unlike `FeedTokenResolver`,
    which *is* installed inside `new`): the `Arc`s these need are built by `hs-cli`'s
    `build_session_mounts` *after* `user` (this hub) already exists -- `push` is constructed later
    in that same function, over the same backend. Making these installs opt-in calls rather than
    constructor arguments means `hs-cli` needed **zero** changes for this session's work to compile
    and every existing test/caller to keep working unchanged; only *using* the feature in the real
    server needs the three-line addition documented below.
- **`crate::sync::build`** (`crates/hs-user/src/sync/mod.rs`): computes `push_rules_for_sync` once
  per response via `hub.push_rules_store()`, pushes `{"type": "m.push_rules", "content": ...}`
  into the same `global_account_data_json` vec the existing account-data path already builds
  (unconditionally on an initial sync, gated on `changed_seq > baseline.push_rules_seq` on an
  incremental one), and threads `push_rules_seq: new_push_rules_seq` into the outgoing token via
  `.max(baseline.push_rules_seq)` -- the identical shape `new_account_data_seq` already used.
  `crate::sync::has_new_data` (the long-poll wake check) gained the matching
  `push_rules.store().changed_seq(user_id).await? > baseline.push_rules_seq` branch, reading
  through the cache via `CachedRulesetStore::store()` exactly as track 10's recipe specified. Per
  room, the hardcoded `"unread_notifications": {"highlight_count": 0, "notification_count": 0}`
  and `"unread_thread_notifications": {}` are now `hub.counts_store()`'s
  `get_room_counts(user_id, room_id).await?.totals()` and a real per-thread map, falling back to
  the same all-zero `RoomNotificationCounts::default()` when no store is installed.
- **`UserError` gained `Push(#[from] hs_push::error::StoreError)`**, mapped to `500` like every
  other backend-failure variant (`crates/hs-user/src/error.rs`) -- `hs-push`'s stores can now fail
  a `/sync` build the same way `hs-e2e`'s already could.

### 2. `GET /keys/changes` resolves this crate's own `/sync` tokens

`crates/hs-e2e/src/routes/keys_changes.rs`'s `resolve_stream_pos` already tries a plain decimal
first, then falls back to `E2eState::sync_token_resolver()`'s installed
`hs_e2e::state::SyncTokenResolver` if any. Added `crate::hub::DeviceListTokenResolver` (a
zero-field unit struct -- no store lookup needed at all, unlike `FeedTokenResolver`: a device-list
stream position *is* one of `SyncToken`'s own fields verbatim, so decoding the token answers the
question outright) implementing that trait, and `SessionHub::install_device_list_token_resolver(&self,
e2e: &hs_e2e::state::E2eState<B>)` to install it. Same "not a `new` side effect" reasoning as the
push-rules stores above: the `E2eState` this needs is a sibling of this hub in `hs-cli`'s
construction, not an input to it.

### 3. Room summaries (`summary`, previously hardcoded `{}`)

`crate::sync::build_room_summary` (`crates/hs-user/src/sync/mod.rs`, new function): walks
`actor.members()` once per room, counting `join`/`invite` memberships into
`m.joined_member_count`/`m.invited_member_count` and collecting non-self join/invite user IDs into
a `BTreeSet` for `m.heroes`. **Heroes are only populated when the room has neither `m.room.name`
nor `m.room.canonical_alias` set** (checked via two `actor.state_event(...)` calls) -- mirrors
Synapse's own reasoning (heroes exist purely so a client can synthesize a name; a room that
already has one needs none) and every real client's own name precedence. Wired into the existing
per-room `handle.query(move |actor| ...)` closure that already builds `timeline`/`state` (now
returns a 3-tuple), so no extra room-actor round trip.

**Documented simplification**: heroes are ordered lexicographically by user ID (via the `BTreeSet`),
not by "oldest membership" (Synapse's own tiebreak, which needs per-member join-order bookkeeping
this crate does not have and the spec does not mandate). Up to 5 heroes, matching the spec's own
example count.

### Tests added (7 new, `crates/hs-user`: 60 -> 67 unit tests; `sync_scenario.rs` unchanged at 3)

- `token::tests`: existing tests extended for the new field/version rather than new tests (every
  struct literal and length constant updated).
- `sync::tests::push_rules_are_carried_on_an_initial_sync_once_a_store_is_installed`
- `sync::tests::push_rules_only_repeat_on_an_incremental_sync_once_the_ruleset_changes` (proves the
  change-seq gate in both directions: silent when unchanged, reappears after `set_ruleset`)
- `sync::tests::unread_notifications_reflect_an_installed_counts_store`
- `sync::tests::room_summary_reports_heroes_and_counts_for_an_unnamed_room` (also asserts the
  syncing user is excluded from her own heroes list)
- `sync::tests::room_summary_has_no_heroes_once_the_room_has_its_own_name`
- `hub::tests::device_list_token_resolver_decodes_a_sync_token_and_rejects_anything_else`
- `hub::tests::a_second_install_of_the_push_rules_or_counts_store_is_ignored`

Two of the new sync tests initially flaked against the documented "discovery gap" (`crate::hub`'s
module docs: a room's *creation* update publishes before `watch_room` subscribes to it, so a room
with no follow-up event after `watch_room` never gets a membership record) -- fixed by adding a
trivial follow-up `send_event` after `watch_room`, the same pattern
`filter_rooms_allowlist_excludes_other_rooms` (an existing test) already documents and relies on.
Not a bug in this session's new code; a pre-existing test-harness gotcha this session's tests
tripped over like several before them.

### `hs-cli` wiring needed (not made this session -- `hs-cli` was off limits)

`crates/hs-cli/src/serve.rs`'s `build_session_mounts` builds `user`, `e2e` and `push` in that
order over one shared backend, but never connects them. Add exactly this, immediately before that
function's `Ok((user, e2e, push))`:

```rust
// hs-user (track 05) consumes hs-push's stores and hs-e2e's state once all three exist here.
user.hub.install_push_rules_store(push.rulesets.clone());
user.hub.install_counts_store(push.counts.clone());
user.hub.install_device_list_token_resolver(&e2e);
```

All three methods are `pub`, idempotent, and take exactly the types `user`/`push`/`e2e` already
hold in that function (`Arc<CachedRulesetStore<TablesRulesetStore<B>>>`, `Arc<dyn CountsStore>`,
`&E2eState<B>`) -- no new imports needed beyond what `hs-cli` already has. Until this lands, `/sync`
behaves exactly as it did before this session (no `m.push_rules`, hardcoded-zero
`unread_notifications`, `GET /keys/changes` still 400s on an `hsu1_...` token) -- verified via the
loadgen scenario below, which logs this as a named `KNOWN BUG` rather than failing.

### Verification

```
cargo fmt -p hs-user -p hs-loadgen                                    # clean
cargo clippy -p hs-user -p hs-loadgen --all-targets -- -D warnings    # clean
cargo test -p hs-user                                                 # 67 unit + 3 integration = 70 passed
cargo build -p hs-cli --bin hs                                        # compiles unchanged (no hs-cli edits)
cargo test -p hs-loadgen --test real_client -- --nocapture            # 23 steps, passed (below)
cargo test -p hs-loadgen --test real_client_encrypted -- --nocapture  # 16 steps, passed, still decrypts
```

**`real_client` run** (23 steps, up from 22 -- the new push-rules step added; note step 15, profile
propagation, now *hard*-succeeds where session 4 logged it as `KNOWN BUG`: track 04/07 fixed it
since, unrelated to this session):

```
registered @loadgen-alice:hs-loadgen.test
registered @loadgen-bob:hs-loadgen.test
logged in @loadgen-alice:hs-loadgen.test on a second device via POST /login
alice created room !nCRsAl5JRnhZw6gvng:hs-loadgen.test
alice invited @loadgen-bob:hs-loadgen.test
@loadgen-bob:hs-loadgen.test joined !nCRsAl5JRnhZw6gvng:hs-loadgen.test
both clients completed a baseline /sync
KNOWN BUG (not this track's crates -- see docs/status/05-sync.md): hs-user's m.push_rules support is implemented but hs-cli's build_session_mounts has not yet wired hs-push's ruleset store onto the session hub, so alice's baseline /sync did not carry m.push_rules
alice sent $-X7ivLZcx1kcmJPL1K3sDHt8CUx_UDp3sKsGmU3UrBk ("hello bob, this is alice")
bob sent $g6gVZA-8b_n0A1_dlHhI02_myEec_HsrYn_zIwg9T-k ("hi alice, bob here")
bob's incremental /sync saw alice's message
alice's incremental /sync saw bob's message
alice's display name round-tripped through GET/PUT /profile
room name and topic changes appeared in /sync's timeline
room membership lists both @loadgen-alice:hs-loadgen.test and @loadgen-bob:hs-loadgen.test
backward /messages page (no `from`, the live end) contains alice's message (10 events)
backward /messages page, paginated from a token /sync handed back (not /messages itself), contains alice's message (10 events)
forward /messages page, paginated from a /sync token issued before any messages, contains alice's message (5 events)
bob's /sync saw alice's typing notice within the bounded wait
carol's /sync saw her invite to !nCRsAl5JRnhZw6gvng:hs-loadgen.test within the bounded wait
bob's /sync saw alice's profile change reflected in her m.room.member event
both clients logged out
post-logout /sync was correctly rejected: the server returned an error: [401 / M_UNKNOWN_TOKEN] 401 Unauthorized M_UNKNOWN_TOKEN: Unrecognised access token
test matrix_rust_sdk_talks_to_a_real_hs_serve ... ok
```

**`real_client_encrypted` run**: unchanged shape, 16 steps, still decrypts end to end (last line:
"53 concurrent /keys/claim calls for alice's device claimed exactly 50 distinct one-time keys with
no double-claim..."). `GET /keys/changes` against a real `hsu1_...` token was not added as a
loadgen step this session: `matrix-rust-sdk` does not expose a way to drive that endpoint directly
through its own sync loop, and without the `hs-cli` wiring above the resolver is not reachable in
the real server anyway. The resolver's own correctness is covered by
`hub::tests::device_list_token_resolver_decodes_a_sync_token_and_rejects_anything_else`.

### Decisions made this session

- **Push-rules and counts stores install via idempotent `SessionHub` methods (`OnceLock`), not
  `SessionHub::new` parameters.** Keeps this session's entire diff inside `hs-user`/`hs-loadgen`
  with zero `hs-cli` changes required to compile (`hs-cli` was off limits) -- see "hs-cli wiring
  needed" above for the three lines still needed to *activate* the feature in a real server.
- **`push_rules_seq`'s wire-format precedent (version bump, not a backward-compatible append)
  followed exactly**, per `token.rs`'s own documented reasoning: this is a greenfield server, no
  client holds a token across a restart.
- **Heroes ordered lexicographically, not by join order.** Documented simplification (see "3. Room
  summaries" above) -- the spec does not mandate an order and this crate tracks no per-member join
  sequence today.
- **Heroes computed only for an unnamed/unaliased room**, not unconditionally. Matches Synapse's
  own behavior and avoids pointless work for the common case (most rooms have a name).
- Two new tests needed a trivial seeding event after `watch_room` to avoid the pre-existing
  "discovery gap" test-harness race (see "Tests added" above) -- not a new production bug, a test
  fixture detail already documented and worked around elsewhere in this same file.

### Interfaces provided (new this session)

- `SessionHub::install_push_rules_store`/`push_rules_store`,
  `SessionHub::install_counts_store`/`counts_store`, `SessionHub::install_device_list_token_resolver`
  (`crates/hs-user/src/hub.rs`) -- all `pub`, all no-ops until called, all consumed today only by
  `hs-cli`'s wiring (not yet added -- see "hs-cli wiring needed" above).
- `crate::hub::DeviceListTokenResolver` implements `hs_e2e::state::SyncTokenResolver`, and
  `m.push_rules`/populated `unread_notifications`/`unread_thread_notifications`/`summary` are new
  content in `/sync`'s existing response shape, not new endpoints.

### Interfaces needed

- **From `hs-cli`**: the three-line wiring in `build_session_mounts` above. Nothing else.
- The profile-propagation gap this crate flagged in session 4 is now fixed (see the `real_client`
  run above, step "bob's /sync saw alice's profile change..." now hard-succeeding) -- track 04/07's
  doing, not this session's; recorded here since session 4's own "Interfaces needed" named it as an
  open ask *of* those tracks.

### What's next for track 05

1. `m.receipt` (read receipts) -- `SyncToken::receipts_seq` has been reserved since session 1 and
   is still unused; the typing/presence/push-rules pattern (registry or store, counter, cursor,
   wake) applies directly. `hs-push`'s `CountsStore::reset` is the documented call a receipt
   advancing past a notifying event should trigger (`docs/status/10-push.md`'s `counts.rs` module
   doc) -- not wired yet since receipts themselves don't exist.
2. Presence's idle/logout-driven automatic offline transition (deferred since session 4, unchanged
   scope cut).
3. Once `hs-cli` adds the three-line wiring above, flip this session's loadgen `KNOWN BUG` step for
   `m.push_rules` to a hard assertion.

## Session 4 (2026-09-19): typing, presence, and the `MustSyncUntil` cluster

**Task**: track 14's status file flagged the single largest remaining Complement failure cluster
as "probably one investigation, not N bugs" -- a broad set of `MustSyncUntil: timed out` failures
across profile updates, typing, device-list changes, invites, presence-in-sync, room summaries
and push-rule carryover, all sharing the shape "a change that is not a timeline event should
appear in the next sync". Also assigned: presence endpoints 404. Scope: `crates/hs-user/**`,
`crates/hs-loadgen/**` and this file; `hs-room`, `hs-auth`, `hs-federation`, `hs-config`,
`hs-push`, `hs-cluster` and `hs-cli` were off limits (five other tracks running concurrently).

### The diagnosis: two unrelated causes, not one

Looked at what actually wakes a long-polling `/sync` before touching anything
(`crate::sync::has_new_data`/`crate::sync::long_poll`, `crate::hub::SessionHub`'s wakers):

1. **`crate::hub::SessionHub::process_room_update`** (driven by `hs_room`'s `RoomUpdate` publish
   stream) is the *only* thing that ever calls `SessionHub::wake`. It fires for timeline events
   and membership deltas -- i.e. anything that is, or accompanies, a persisted room event. This
   covers ordinary messages, joins, invites, kicks, bans, room-state changes (name/topic/etc).
   **Invites already worked** before this session touched anything -- confirmed live (see "Proof"
   below): a third user invited but never joined saw `rooms.invite` in a bounded-wait `/sync` with
   no code change. Complement's `TestRoomsInvite`/federation-package invite failures are therefore
   *not* this bug; they are either the cross-server TLS/signature issues track 14 and track 06
   already diagnosed, or a different, not-yet-isolated cause -- **not the same root cause as the
   rest of this cluster**, contrary to the "probably one investigation" framing. Recorded here so
   the next person doesn't re-diagnose it as a sync-wake problem.
2. **Typing and presence had no representation in this crate at all.** `crate::sync`'s module docs
   said outright: "every room's `ephemeral.events` is always `[]`" and "the top-level
   `presence.events` is always `[]`". There was no waker hook, no cursor, and (for typing) no
   route to set the state in the first place -- `PUT /rooms/{roomId}/typing/{userId}` did not
   exist anywhere in this workspace, confirmed by grep across every crate's `src/routes/mod.rs`.
   This is the real cause behind `TestTyping`/`TestLeakyTyping`, `TestPresenceSyncDifferentRooms`/
   `TestSync`'s presence subtests, and the presence-endpoint 404s (`TestPresence`,
   `TestMembersLocal`). **Fixed this session, entirely within `hs-user`** -- see below.
3. **Device-list changes, room summaries, and push-rule carryover are three more distinct causes,
   diagnosed but not fixed here** (out of this track's crates):
   - `crate::sync::build` already computes `device_lists.changed`/`left` correctly and
     `has_new_data` already peeks `hs_e2e::store::DeviceKeyStore::current_stream_pos` every
     `E2E_POLL_INTERVAL` (500ms) -- that machinery was already correct and unchanged this session.
     Grepped `record_device_list_change` (the only thing that ever bumps that stream): it is
     called from exactly one place, `crates/hs-e2e/src/routes/cross_signing.rs`. Neither key
     upload (`/keys/upload`) nor device rename/delete (`hs-auth`'s device routes) call it. A
     `TestDeviceListUpdates`-style scenario that renames a device or uploads new keys (rather than
     bootstrapping cross-signing) will never see a stream bump to notice in the first place --
     **this is track 08's (`hs-e2e`) or track 07's (`hs-auth`) call site, not a sync-side bug.**
   - Room summaries (`TestRoomSummary`) and push-rule carryover are both simply unbuilt: `/sync`'s
     `summary` key is hard-coded `{}` (`crate::sync::build`, the `join` bucket's JSON literal), and
     no push rule has ever appeared in a sync response at all (`TestPushSync`, confirmed
     separately by track 14). Room summary computation (heroes, joined/invited counts) is squarely
     this crate's job and is genuinely unbuilt -- recorded here as **not reached this session**,
     next for track 05. Push-rule carryover on upgrade is track 10's.
   - Profile updates: see the dedicated section below -- diagnosed in detail, not this crate's fix.

**Conclusion for track 14's framing**: not one investigation. Two causes squarely in this
crate's ownership (typing, presence -- fixed), one already-working path that was misattributed
to this cluster (invites), and three more distinct causes in other tracks' crates (device-list
call sites, room summaries [partially ours, unbuilt], push-rule carryover).

### The fix: `m.typing` and `m.presence`, both wired into the wake path for real

New modules, both in-memory (never persisted -- both are genuinely ephemeral; Synapse doesn't
persist them either):

- **`crates/hs-user/src/typing.rs`**: `TypingRegistry`, keyed by room. A `typing: true` call
  inserts a deadline (capped at `MAX_TYPING_TIMEOUT` = 120s regardless of what the client asked
  for); `typing: false` removes it. A single global monotonic counter is stamped onto whichever
  room changed -- this is what lets `SyncToken::typing_seq` (new field, see below) answer "has
  *this* room changed since my last sync" without the token growing per-room. Expiry is pruned
  *lazily* on read (`TypingRegistry::current`), not by a spawned timer per typing user; a prune
  that actually removes someone also bumps the counter, so an already-synced client blocked in a
  long poll is woken by a typing *timeout* the same way it's woken by an explicit `typing: false`.
- **`crates/hs-user/src/presence.rs`**: `PresenceRegistry`, keyed by user. Same shape (global
  counter, `SyncToken::presence_seq` -- a field the token format had reserved since session 1,
  before this module existed). No expiry: a user's presence is exactly what they last set it to,
  forever, until they set it again (see "Deferred" below).
- **`crates/hs-user/src/hub.rs`**: `SessionHub::set_typing`/`SessionHub::typing_users`,
  `SessionHub::set_presence`/`SessionHub::presence_of`, and a new public
  `SessionHub::users_sharing_room_with` (the same membership walk `crate::sync::shared_users`
  already did for `device_lists` -- that function now delegates to this instead of duplicating
  it). Both `set_typing` and `set_presence` **call the hub's existing `Notify` waker directly**
  for every affected user, immediately -- unlike to-device/device-list activity (which has no
  waker hook at all and relies on `has_new_data`'s 500ms `E2E_POLL_INTERVAL` peek), a typing or
  presence change wakes a blocked long poll with no poll-interval lag, proven in
  `crates/hs-user/src/sync/mod.rs`'s
  `a_typing_change_wakes_a_long_poll_and_appears_in_ephemeral_events` test (a long poll given a 5s
  timeout returns in well under 2s when a concurrent task sets typing 50ms in).
- **`crates/hs-user/src/routes/typing.rs`**: `PUT /rooms/{roomId}/typing/{userId}`. Only the
  named user may set their own state (`403` otherwise, distinct from a plain "not self" for
  clarity: new `UserError::Forbidden` variant), and only while actually a joined member of the
  room (`403` for an invitee, a past member, or a stranger). Checked the path-conflict question
  the previous (rate-limited, restarted) attempt at this assignment had flagged before dying:
  `hs-room`'s router registers nothing under `/rooms/{roomId}/typing/...` (grepped its full route
  list), so this is a genuinely new leaf in `hs-http`'s `Builder`, not a collision.
- **`crates/hs-user/src/routes/presence.rs`**: `GET`/`PUT /presence/{userId}/status`. `GET` on a
  user this process has never heard a presence update from returns the spec's implied default
  (`{"presence": "offline"}`), not `404` -- `404` would require checking `hs-auth`'s user table
  for existence, which this crate could do (it already depends on `hs-auth` for `UserState::auth`)
  but was judged not worth doing this session for a value few clients ever branch on; recorded as
  a scope note, not a silent gap.
- **`crates/hs-user/src/token.rs`**: `SyncToken` gained `typing_seq` (new field; `presence_seq`
  already existed, reserved since session 1). Wire version bumped `1` -> `2`
  (`SyncToken::decode` now rejects a version-1 token outright) -- safe here since every test and
  every real deployment of this greenfield server restarts from a freshly built binary; no
  long-lived client ever holds a cross-version token. All property tests, and the two fixed-length
  tests that encode a raw byte count, updated for the new 7-`u64` payload.
- **`crates/hs-user/src/sync/mod.rs`**: `build` now gathers each joined room's typing state
  up front (independent of the feed-derived candidate-room set, since a typing-only change never
  touches the feed at all) and folds any room with something new into the candidate set so it
  renders even when nothing else changed; `has_new_data` gained the matching per-room typing check
  and a presence check scoped to `shared_users` (now `SessionHub::users_sharing_room_with`) plus
  self. `presence.events` at the top level is populated for every user sharing a joined room with
  the syncer whose `presence_seq` is newer than the client's baseline (or, on an initial sync,
  every shared user with any record at all).

### Presence: implemented, with one deliberate cut

Presence is fully implemented (endpoints, sync wake, sync events, privacy scoping identical to
`device_lists`). **Not implemented**: Synapse's idle/logout-driven automatic transition to
`unavailable`/`offline` (a background timer watching last-activity). A user's presence is exactly
what they last explicitly set via `PUT .../presence/{userId}/status`, forever, until they set it
again. This is a real spec-completeness gap (`TestPresence`-adjacent tests that check idle
timeout behavior specifically will still fail), but implementing a correct idle-timer subsystem
in the time available would have crowded out the actual `MustSyncUntil`-cluster fix this session
was scoped for; recorded here as the next thing to build for presence, not folded in silently.

### Diagnosis for track 04 / track 07: profile changes never reach `/sync`

**Confirmed live with the real client this session** (see "Proof" below, step 14): `PUT
/profile/{userId}/displayname` now succeeds (track 07 built the route since this crate's last
session), and reading it straight back via `GET /profile` round-trips correctly. But a second
user who already shares a room with the profile owner **never sees the change** -- their `/sync`
never gets an updated `m.room.member` event for that user, no matter how long the bounded wait
runs. Root cause, confirmed by reading both sides:

- `crates/hs-auth/src/routes/profile.rs`'s `put_displayname`/`put_avatar_url` write only
  `UserRecord::display_name`/`avatar_url` in `hs-auth`'s own store. Nothing there touches any
  room's state.
- `crates/hs-room/src/routes/membership.rs` (per that module's own doc comment, confirmed by
  reading it) reads a user's profile out of `hs-auth`'s store **only when minting a *new***
  `m.room.member` event -- i.e. at join/invite/knock time. An *existing* member's already-sent
  `m.room.member` event is never revisited when their profile changes later.

Per the spec (and Synapse's actual behavior), a profile change is supposed to propagate by the
server re-sending each affected room's `m.room.member` event with the same membership but updated
`displayname`/`avatar_url` -- for every room the user is currently joined to. That is a
room-actor write (a new event, `hs-room`'s territory) triggered by a profile-store write
(`hs-auth`'s territory), and belongs to whichever of those two tracks owns "iterate a user's
joined rooms and mint a membership refresh event" -- not to `hs-user`, which only ever consumes
`hs-room`'s publish stream, never originates room events. **Not fixed here.** This is the direct,
confirmed cause of Complement's `TestDisplayNameUpdate`, `TestAvatarUrlUpdate`, and
`user_directory_display_names_test.go`.

### Proof: extended the real-client loadgen scenario (`crates/hs-loadgen/src/scenario.rs`)

Added a `sync_until` helper -- the Complement `MustSyncUntil` pattern in miniature: repeatedly
`/sync`s a client, chaining `next_batch` into the next `since` exactly like a real client's sync
loop, until a predicate matches or a bounded wait elapses. Unit tests inside `hs-user` already
proved the *pieces* work in isolation; only a real client doing exactly what Complement does --
bounded polling of the real long-poll endpoint over a real socket, from a second, independent
process's connection -- can prove the *wake* actually reaches another client's blocked `/sync`.

Three new steps (12-14, renumbering the old "12. Log out" to "15"):

- **Step 12 (typing, hard assertion)**: alice sends a typing notice; bob's next bounded-wait
  `/sync` must see it in `ephemeral.events` within 10s. This is this session's own new code, so a
  failure here would be a real regression, not a documented gap.
- **Step 13 (invite-before-join, hard assertion)**: a third user, carol, is invited but never
  joins; her bounded-wait `/sync` must show the room under `rooms.invite` within 10s. Confirms the
  "invites already work" half of the diagnosis above with a fresh user who has literally never
  synced before (the strongest form of the claim).
- **Step 14 (profile propagation, soft-failed like the existing step 8)**: alice changes her
  display name again; bob's bounded-wait `/sync` (5s) is checked for an updated `m.room.member`
  event naming the new display name, in either `state` or `timeline`. Logged as `KNOWN BUG` (not
  this track's crates) rather than a hard failure, so the scenario stays green while the gap
  documented above remains open in `hs-auth`/`hs-room`.

**Actual run, `cargo build -p hs-cli --bin hs && cargo test -p hs-loadgen --test real_client --
--nocapture`** (22 steps, all logged, scenario passed):

```
registered @loadgen-alice:hs-loadgen.test
registered @loadgen-bob:hs-loadgen.test
logged in @loadgen-alice:hs-loadgen.test on a second device via POST /login
alice created room !mD6FQhVXSN7GmclY79:hs-loadgen.test
alice invited @loadgen-bob:hs-loadgen.test
@loadgen-bob:hs-loadgen.test joined !mD6FQhVXSN7GmclY79:hs-loadgen.test
both clients completed a baseline /sync
alice sent $H-PGDNJOql5goN8G5IsW-dOPFCWORm0bBeO06eGjAx4 ("hello bob, this is alice")
bob sent $nnGT2vVRFEvcat44Z2DZMFw9ZNfVLvOzxjdWy__2nYM ("hi alice, bob here")
bob's incremental /sync saw alice's message
alice's incremental /sync saw bob's message
alice's display name round-tripped through GET/PUT /profile
room name and topic changes appeared in /sync's timeline
room membership lists both @loadgen-alice:hs-loadgen.test and @loadgen-bob:hs-loadgen.test
backward /messages page (no `from`, the live end) contains alice's message (10 events)
backward /messages page, paginated from a token /sync handed back (not /messages itself), contains alice's message (10 events)
forward /messages page, paginated from a /sync token issued before any messages, contains alice's message (4 events)
bob's /sync saw alice's typing notice within the bounded wait
carol's /sync saw her invite to !mD6FQhVXSN7GmclY79:hs-loadgen.test within the bounded wait
KNOWN BUG (not this track's crates, see docs/status/05-sync.md): alice's profile change never reached bob's /sync as an updated m.room.member event within the bounded wait
both clients logged out
post-logout /sync was correctly rejected: the server returned an error: [401 / M_UNKNOWN_TOKEN] 401 Unauthorized M_UNKNOWN_TOKEN: Unrecognised access token
test matrix_rust_sdk_talks_to_a_real_hs_serve ... ok
```

Note: step 12 ("alice's display name round-tripped...") now hard-succeeds where session 1's
writeup recorded it as a `KNOWN BUG` -- track 07 built the route since then. The soft-fail
wrapper around it is now dead code from this crate's point of view (no observed failure this
session) but left in place since this crate does not own `hs-auth` and cannot guarantee it stays
mounted; see "Next" below.

**No regressions**: `cargo test -p hs-loadgen --test real_client_encrypted` still decrypts end to
end (16 steps, unchanged from session 2's writeup, re-run this session to confirm); `cargo test
-p hs-user` (60 tests, up from the count before this session's new modules/tests) is green.

### Verification commands run this session

```
cargo fmt -p hs-user -p hs-loadgen                                    # clean
cargo clippy -p hs-user -p hs-loadgen --all-targets -- -D warnings    # clean (see note below)
cargo test -p hs-user                                                 # 60 passed
cargo build -p hs-cli --bin hs
cargo test -p hs-loadgen --test real_client -- --nocapture             # 22 steps, passed (above)
cargo test -p hs-loadgen --test real_client_encrypted -- --nocapture   # 16 steps, passed, unchanged
```

Note on `cargo clippy ... -D warnings`: this repeatedly failed transiently on an *unrelated*
pre-existing `unused import`/(briefly) a missing-struct-field compile error in `hs-admin`
(track 15, actively being edited this session) -- clippy lints every local path dependency, not
just the `-p` targets, and `hs-admin` is one, exactly the same class of cross-track interference
session 3's writeup already recorded for `hs-room`. Retried every ~20s; resolved on its own within
a few attempts each time, same as before. Not this track's bug, not fixed here, recorded so a
future reader doesn't mistake it for a `hs-user`/`hs-loadgen` regression.

### Decisions made this session

- **`SyncToken`'s wire version bumped 1 -> 2** to add `typing_seq`. A version-1 token now fails to
  decode. Judged safe (see `token.rs`'s updated module doc) since nothing in this workspace holds
  a token across a server restart today.
- **Typing timeout capped at 120s server-side** (`crate::typing::MAX_TYPING_TIMEOUT`), regardless
  of what a client requests, to bound how long a misbehaving client can pin an entry.
- **Typing expiry is pruned lazily on read, not by a spawned timer.** A prune that removes
  someone still bumps the shared counter, so it's indistinguishable from an explicit change for
  wake purposes. Simpler and cheaper than a timer per typing user; the tradeoff is that "someone
  stopped typing" is only noticed the next time anything reads that room's typing state (a sync
  build, or a long poll's 500ms `E2E_POLL_INTERVAL` recheck) rather than the instant the deadline
  passes -- acceptable since typing timeouts (tens of seconds) are far longer than that interval.
- **`GET /presence/{userId}/status` requires auth but does not restrict *whose* presence can be
  queried**, and returns `{"presence": "offline"}` (not `404`) for a user this process has never
  heard a presence update from. Both are spec-permitted defaults; documented as a scope choice
  rather than silently relying on it.
- **No automatic idle/logout-driven presence transitions** (see "Presence" above) -- a deliberate
  scope cut, not an oversight, given the session's actual assignment.
- **`hs_user::sync::shared_users` now delegates to a new public `SessionHub::users_sharing_room_with`**
  rather than duplicating the membership walk a second time for presence's identical privacy
  scope requirement.

## Interfaces provided (new this session)

- `PUT /rooms/{roomId}/typing/{userId}` (`crate::routes::typing::put_typing`).
- `GET`/`PUT /presence/{userId}/status` (`crate::routes::presence::get_status`/`put_status`).
- `SessionHub::set_typing`/`typing_users`, `SessionHub::set_presence`/`presence_of`,
  `SessionHub::users_sharing_room_with` -- all `pub`, usable by another crate that gets a
  `SessionHub` handle (none does today; recorded for completeness).
- Both are additions to the existing `/sync` response shape (`ephemeral.events`, top-level
  `presence.events`), not new endpoints for any other track to integrate against.

## Interfaces needed (unchanged from session 3 unless noted)

- Still nothing new from another track for this session's work. The profile-propagation gap
  (above) is a need pointed *at* track 04/07, not a need *of* this track.

## What's next for track 05

1. **Room summaries** (`summary` key, hard-coded `{}` today) -- heroes, joined/invited member
   counts. Confirmed unbuilt this session (see "Diagnosis" above); squarely this crate's job.
2. Presence's idle/logout-driven automatic offline transition (see "Presence" above).
3. `m.receipt` (read receipts) -- `SyncToken::receipts_seq` has been reserved since session 1 and
   is still unused; the same typing/presence pattern (registry, counter, cursor, wake) applies
   directly.
4. If track 04 or track 07 picks up the profile-propagation fix diagnosed above, remove the soft-
   fail wrapper around loadgen scenario step 14 and assert it hard.

## Session 3 (2026-09-19): a real client can scroll back after syncing -- joint 04/05

**Task**: every real Matrix client's ordinary flow is sync, then paginate backward from the token
`/sync` just handed it. `hs-user`'s `/sync` mints `hsu1_...` tokens; `hs-room`'s
`GET /rooms/{roomId}/messages?from=` parsed only its own room-local `crate::timeline::PaginationToken`
(`<f|b><room_pos>`) and rejected `hsu1_...` outright with `400 M_INVALID_PARAM`. Complement's own
`room_messages_test.go` hits exactly this (`TestSendAndFetchMessage` and several `TestLeftRoomFixture`
subtests feed a bare `/sync` `next_batch` straight into `/messages?from=`), and track 04's own
session-4 status writeup flagged it as a joint 04/05 design question, not a quick patch (see that
file's "What is left"/"Interfaces needed"). Scope for this session: `crates/hs-user/**`,
`crates/hs-room/**`, `crates/hs-loadgen/**` and this file, per the joint-session brief; `hs-cli`,
`hs-e2e`, `hs-media`, `hs-kv` and `hs-cluster` were off limits (other tracks in flight there).

### The decision, made before implementing it

**Two token formats stay, with an explicit, tested conversion at the boundary -- not one shared
wire format.** What each format encodes, and why merging them was rejected:

- `hs-user`'s `/sync` token (`crate::token::SyncToken`, `hsu1_...`) encodes a **per-user** position
  (`feed_seq`, an index into that one user's own coalesced, cross-room feed) plus five small
  independent cursors (to-device, device-list, account-data, presence, receipts). It is
  deliberately *not* a vector of per-room positions -- `crate::token`'s own module doc explains why
  at length: a token that grew with the number of rooms a user is in would defeat the point of an
  opaque, constant-size token a client stores and echoes back. This is not a Phase-0 shortcut to
  revisit; it is the whole reason `/sync` scales to a power user with thousands of rooms.
- `hs-room`'s pagination token (`crate::timeline::PaginationToken`, e.g. `b42`) encodes a
  **room-local** timeline position (`room_pos`, an `i64` the room's own actor assigns, monotonic
  *within that room only*, unrelated to any other room's numbering) plus a direction. This is what
  `RoomActor::paginate`'s `BTreeMap`-backed timeline actually indexes by, and it has no notion of
  "which user" at all -- pagination position is the same for every reader of a room.
- **These are genuinely different axes, not two encodings of the same fact.** A `feed_seq` cannot
  be turned into a `room_pos` by decoding bytes differently; it requires per-user, per-room
  bookkeeping (`crate::store::UserStore::room_pos_as_of`, `hs-user`'s own durable feed) that
  `hs-room` has no reason to duplicate and no way to reach without depending on `hs-user` --  which
  would be a cycle, since `hs-user` already depends on `hs-room` (to render room timelines via
  `hs_room::actor::RoomActor` inside `/sync` itself). Making the two crates mint literally the same
  wire format was therefore not just extra work but architecturally backwards: it would mean
  either baking a per-room position vector into `/sync`'s token (the exact bloat `SyncToken`'s
  design rejects) or making `hs-room`'s pagination carry `hs-user`'s five unrelated cursors for no
  reason a room-only reader would ever need.
- **What resolves the mismatch: `hs-room`'s `GET /messages` now takes an optional hook**
  (`hs_room::registry::GlobalTokenResolver`, installed on `RoomRegistry` via the new
  `install_global_token_resolver`) that a token-minting crate can implement to translate its own
  opaque string into a room-local position, without either crate depending on the other's types.
  `hs-room` defines the trait (it is the *consumer*: `crate::routes::query::get_messages` tries its
  own `PaginationToken::from_str` first, and only on failure asks the installed resolver "do you
  recognize this token, and if so, what room-local position does it mean for this user in this
  room?"). `hs-user` implements it (`crate::hub::FeedTokenResolver`, decoding `SyncToken` and
  calling `UserStore::room_pos_as_of`, with the same membership-position fallback
  `crate::sync::resume_mode` already uses for a room with no feed entry at or before the token) and
  installs one on every `SessionHub::new` call -- see "How it gets wired into the real server"
  below for why that particular call site, not `hs-cli`, does the installing.
- **Rejected alternative: move `GET /messages` into `hs-user` entirely.** Tried first, actually --
  `hs-user` already has everything needed (a `RoomSource<B>` over the same `RoomRegistry`, plus its
  own feed store) to re-implement the handler by calling `hs-room`'s already-`pub` `RoomActor`
  primitives (`can_read_room`, `paginate`, `event_visible_to`, `relation_bundle`) directly, the way
  `crate::sync::build_incremental_timeline` already does for `/sync`'s own room timelines. This
  was abandoned once it broke `crates/hs-room/tests/scenario.rs`, which builds and drives
  `hs_room::routes::router()` **standalone** (no `hs-user` merged in) and asserts on `/messages`
  directly -- unmounting the route from `hs-room`'s own router to move it elsewhere would have
  meant either breaking that test suite outright or rewriting it to pull in `hs-user`, a much
  larger and riskier change for a session scoped to a token-format mismatch. The
  `GlobalTokenResolver` hook gets the same practical result (a token minted by `/sync` works
  against `/messages`) without moving the route or touching `hs-room`'s own test harness at all --
  `cargo test -p hs-room` still exercises the exact same router it always has, 48/48 green.
- **What happens to tokens already issued by a running server.** Nothing breaks, in either
  direction: an already-issued `hs-room`-native `PaginationToken` string (`b42`) still parses via
  `PaginationToken::from_str` first, exactly as before this session, and never reaches the resolver
  at all. An already-issued `hs-user` `SyncToken` (`hsu1_...`) that could not previously be used
  against `/messages` now can; nothing that used to work stops working, and there is no wire-format
  version bump on either side (`SyncToken`'s own `VERSION` byte and `PaginationToken`'s own shape
  are both untouched). A token that matches neither format still gets the same `400
  M_INVALID_PARAM: invalid pagination token` it always did.
- **Pagination boundary semantics: exclusive of the resolved position, in both directions**,
  matching the *existing* room-scoped `timeline.prev_batch` `/sync` already hands out
  (`crate::sync::build_incremental_timeline`'s `PaginationToken::new(resume_pos, Backward)`, never
  changed by this session). A token minted right after a client observed message M pages backward
  to whatever is *older* than M and forward to whatever is *newer*, never re-returning M itself --
  the client already has M from the sync response that handed it the token. Verified directly in
  `crates/hs-user/tests/sync_scenario.rs::messages_accepts_a_token_minted_by_sync_in_both_directions`
  (below).

### Implementation

- **`crates/hs-room/src/registry.rs`**: new `GlobalTokenResolver` trait (`async fn resolve(&self,
  user_id, room_id, raw: &str) -> Result<Option<Option<i64>>, RoomError>` -- outer `None` means "not
  my token format", `Some(None)` means "my format, but no position for this room", `Some(Some(pos))`
  is a resolved room-local position), a new `OnceLock<Arc<dyn GlobalTokenResolver>>` field on
  `RoomRegistry`, and `RoomRegistry::install_global_token_resolver`/`global_token_resolver`. Added
  `async-trait` to `hs-room`'s own `[dependencies]` (already in the workspace's
  `[workspace.dependencies]`, just not previously used by this crate -- noted under "Shared
  dependencies added").
- **`crates/hs-room/src/routes/query.rs::get_messages`**: `from` is tried as `PaginationToken`
  first (unchanged behavior for a token this endpoint or `/sync`'s room-scoped `prev_batch` already
  mints); on parse failure, if a resolver is installed, its three-way answer is used (not
  recognized -> `400`; recognized but unresolved -> treated as an absent `from`; resolved ->
  a `PaginationToken` at that position, in the requested direction). No behavior change at all when
  no resolver is installed (a test harness that constructs `RoomRegistry` directly and never touches
  `hs-user`, like `crates/hs-room/tests/scenario.rs`, sees exactly the pre-session behavior).
- **`crates/hs-user/src/room_source.rs`**: `RoomSource<B>` gained a new trait method,
  `install_global_token_resolver`, defaulted to a no-op (a test double that never touches
  `hs-room`'s HTTP layer has nothing to install it on) and overridden for `Arc<RoomRegistry<B>>` to
  forward to `RoomRegistry::install_global_token_resolver`.
- **`crates/hs-user/src/hub.rs`**: new private `FeedTokenResolver` implementing
  `hs_room::registry::GlobalTokenResolver` by decoding `raw` as a `SyncToken` and resolving via
  `UserStore::room_pos_as_of`, falling back to the user's last recorded membership position for the
  room (mirroring `crate::sync::resume_mode`'s own fallback), then finally "recognized, no
  position". `SessionHub::new` now installs one of these on `rooms` (its `RoomSource<B>`) as a
  side effect of construction.
- **How it gets wired into the real server, without touching `hs-cli`.** This session's brief
  forbids editing `crates/hs-cli` (other tracks in flight there), and `RoomState<B>`/`UserState<B,
  R>` are both constructed by call sites inside `hs-cli` that this session cannot change the
  arity of. Installing the resolver as a side effect of `SessionHub::new(store, rooms,
  fan_out_threshold)` -- a function whose call site in `hs-cli` (`build_session_mounts`) already
  passes it the exact same `Arc<RoomRegistry<B>>` (as `rooms`, monomorphizing `R`) that gets handed
  to `RoomState<B>` right next to it -- means the wiring happens automatically the moment `hs-cli`'s
  existing, completely unmodified code runs. This mirrors the workspace's established "leave a
  clear seam, don't require a coordinated edit" convention (the same one that resolved the
  `/publicRooms` collision between these two crates last session): the fix is entirely inside
  `hs-user`'s and `hs-room`'s own crates, and the real server picks it up with no `hs-cli` change
  at all -- confirmed by `cargo build -p hs-cli --bin hs` succeeding unchanged and the loadgen
  proof below running against that exact binary.

### Proof

**Fast, in-process proof** (`crates/hs-user/tests/sync_scenario.rs::messages_accepts_a_token_minted_by_sync_in_both_directions`,
new this session): sends an older message, takes a `/sync` token, sends a newer message, takes
another `/sync` token, then: `dir=f` from the pre-newer-message token finds the newer message and
not the older one; `dir=b` from the post-newer-message token finds the older message and not the
newer one (see "Pagination boundary semantics" above for why each direction excludes exactly the
message it does); a token minted by `/messages` itself still works (backward compatibility); an
outright malformed token still gets a clean `400`, not a panic. All pass.

**Real-client proof against the actual binary**, per this session's mandate ("prove it with the
real client, not a unit test"). `crates/hs-loadgen`'s scenario (`crates/hs-loadgen/src/scenario.rs`,
step 11) previously paginated `/messages` with `MessagesOptions::backward()` and no `from` at all
(`from: None` sends no `from` query parameter whatsoever -- the exact gap that let this bug survive
every previous session's run of this scenario, since it never actually exercised token parsing).
Extended to page using the token `alice_sync_2.next_batch` (minted by a real `/sync` call, after
alice's message, the room rename and the topic change) for `dir=b`, and a token captured before any
message was sent (`alice_baseline_token`, cloned out before its original binding was consumed by an
earlier `SyncSettings::token(...)` call) for `dir=f`.

Run with:
```
cargo build -p hs-cli --bin hs
cargo test -p hs-loadgen --test real_client -- --nocapture
```

Relevant step lines from an actual run (19/19 steps, full log has every step):
```
- backward /messages page (no `from`, the live end) contains alice's message (10 events)
- backward /messages page, paginated from a token /sync handed back (not /messages itself), contains alice's message (10 events)
- forward /messages page, paginated from a /sync token issued before any messages, contains alice's message (4 events)
```

### Verification, all clean this session

```
cargo fmt -p hs-user -p hs-room -p hs-loadgen
cargo clippy -p hs-user -p hs-room -p hs-loadgen --all-targets -- -D warnings
cargo test -p hs-room                                    # 48/48 (38 lib/unit + 10 scenario), unchanged count
cargo test -p hs-user                                    # 41 lib/unit + 3 sync_scenario (was 2; +1 new), all green
cargo build -p hs-cli --bin hs
cargo test -p hs-loadgen --test real_client -- --nocapture              # 19/19 steps (was 17; +2 new)
cargo test -p hs-loadgen --test real_client_encrypted -- --nocapture    # unchanged, still green -- see its own
                                                                          # step list: E2EE end-to-end decryption
                                                                          # and the 53-caller /keys/claim atomicity
                                                                          # probe both still pass, confirming this
                                                                          # session did not regress track 08's work.
```

`cargo test -p hs-cli --test e2e` **could not be run to completion this session**: `hs-cli`'s own
lib fails to compile independent of anything touched here (`crates/hs-cli/src/audit.rs`, a new,
untracked file another track is actively adding concurrently, has a genuine type error at its own
`highest_sequence` -- `TupleKey::decode` called with a `&(u64, String)` where it wants `&[u8]`).
Confirmed not caused by this session: nothing in this session's diff touches `hs-cli` at all (out
of scope per this session's brief), and `cargo build -p hs-cli --bin hs` succeeded cleanly earlier
in this same session, before that file's concurrent edit landed. Whoever owns `hs-cli`/the admin
audit log next should rerun `cargo test -p hs-cli --test e2e` once `audit.rs` compiles again --
nothing in this session's own testing gives any reason to expect it to fail.

### Bonus items (from the joint session's "if you have time" list): not reached

Both remaining items on track 04's own "What is left" -- `GET /context`'s `state` field still
reading live current state instead of state pinned to the target event, and `hs-user`'s
`hub.rs::public_directory_entry` inferring "public" from the join rule instead of calling
`RoomRegistry::is_directory_public` -- were explicitly lower priority than the token-format fix
and its proof, and were not reached this session given the time the token-format design question
and the `hs-cli` compile blocker above took to resolve and verify. Both remain open; see track 04's
own status file for the exact fix each implies (`RoomActor::state_at_event` for the first;
`RoomRegistry::is_directory_public`, already `pub`, for the second).

### Interfaces provided (session 3)

- `hs_room::registry::GlobalTokenResolver` (new trait) and
  `RoomRegistry::{install_global_token_resolver, global_token_resolver}` (new methods) -- any
  crate that mints its own opaque pagination-adjacent token and wants `hs-room`'s `/messages` to
  accept it can implement this trait and install one, the same way `hs-user` now does.
- `hs_user::room_source::RoomSource::install_global_token_resolver` (new trait method, defaulted):
  any future alternate `RoomSource<B>` implementation (a federation or cluster-forwarding shim,
  per that trait's own module doc) inherits the no-op default unless it overrides it.

### Interfaces needed (session 3)

None new from this session's own work. Track 04's two still-open items above remain on that
track's "Interfaces needed" list, unchanged by this session.

### Decisions made (session 3)

See "The decision, made before implementing it" above for the full writeup (two token formats,
explicit resolver-hook conversion at the boundary, rejected alternatives, and what happens to
already-issued tokens). Additionally:

- **The resolver is installed by `SessionHub::new`, not by a new step in `hs-cli`.** See "How it
  gets wired into the real server" above -- this session could not edit `hs-cli` at all, so the
  wiring had to be a side effect of a call site `hs-cli` already makes unchanged.
- **`crates/hs-room/tests/scenario.rs` was left completely untouched.** It was the deciding
  argument against moving `/messages` into `hs-user` -- see "Rejected alternative" above.

### Shared dependencies added (session 3)

- `hs-room/Cargo.toml`: `async-trait.workspace = true` (already present in the root
  `[workspace.dependencies]`; not previously used by this crate). Needed for
  `GlobalTokenResolver`'s `#[async_trait::async_trait]`.

---

## Session 2: making E2EE actually work end to end

**Task**: track 08 drove two real encrypting `matrix-sdk` clients against the real binary and
found that every `hs-e2e` route worked, but the recipient could never decrypt a message because
`GET /sync` omitted `to_device`, `device_lists`, `device_one_time_keys_count` and
`device_unused_fallback_key_types` entirely (see `docs/rfcs/0013-e2ee-sync-extensions.md` and
`docs/status/08-e2ee.md`). This session wires all four in.

### Done

- **`to_device`** (`crates/hs-user/src/sync/mod.rs`, `build`): calls
  `hs_e2e::store::ToDeviceStore::delete_up_to`/`poll_since` keyed off
  `SyncToken::to_device_seq` (a field that already existed on the token, unused until now -- no
  `token.rs` change was needed). **Deletion semantics**: on a request presenting token `T`,
  `delete_up_to(T.to_device_seq)` runs *before* polling for anything new. `T.to_device_seq` is the
  cursor a *previous* response handed this same device; the client presenting `T` back is the
  proof (the ordinary "echo the last `next_batch`" contract) that that previous response was
  received, so it is safe to delete everything up to it. It is **not** safe to delete what the
  *current* response is about to return (those messages have stream ids strictly greater than
  `T.to_device_seq`, so `delete_up_to(T.to_device_seq)` never touches them). Concretely: two
  `/sync` calls with the *same* `since` redeliver the same to-device messages; only presenting the
  *next* token (whose `to_device_seq` covers them) makes them disappear. Tested both directions in
  `sync::tests::to_device_message_is_redelivered_on_the_same_token_and_gone_after_the_next`.
- **`device_one_time_keys_count`/`device_unused_fallback_key_types`**: straight passthrough of
  `OneTimeKeyStore::count_one_time_keys`/`FallbackKeyStore::unused_fallback_key_algorithms` for
  the responding device, populated on *every* response (including the initial sync) once a device
  is known -- not only when non-empty, per the RFC's evidence that an absent
  `device_one_time_keys_count` makes `matrix-sdk-crypto`-style clients assume zero keys and
  re-upload a full batch every sync. Tested in
  `sync::tests::one_time_key_and_fallback_counts_are_populated_on_the_initial_sync`.
- **`device_lists.changed`/`left`**: `DeviceKeyStore::changed_users_since(baseline.device_list_seq,
  Some(current_stream_pos))` intersected with a new helper, `shared_users` (`sync/mod.rs`), which
  computes every user the syncing user currently shares a *joined* room with from this crate's own
  membership data (`hs-e2e` has no room-membership notion at all by design). Only computed on an
  incremental sync (`since` present), matching the spec's "only present on an incremental sync".
  `left` is built from `m.room.member` leave/ban events (for someone other than the syncing user)
  seen in this response's own timelines, minus whoever is still in the current shared set --
  reused from data the room loop was already computing, no extra pass over history. Tested with a
  user who shares no rooms (`device_lists_changed_is_scoped_to_users_who_share_a_room`) and a user
  who leaves the only shared room (`device_lists_left_reports_a_user_who_left_the_only_shared_room`).
- **Long-poll wake condition extended for e2e activity** (`has_new_data`/`long_poll`,
  `sync/mod.rs`): `matrix-sdk`'s `sync_once` sends `timeout=30000` on every incremental sync,
  including one where the only thing that changed is a to-device message or a device-list update
  (no room event at all). The hub's `Notify`-based waker only fires on room activity (wired from
  `hs-room`'s update stream), and this crate cannot add a wake hook to `hs-e2e`'s routes (out of
  scope this session). Rather than block for the full 30s on every such sync, `long_poll` now also
  re-checks `has_new_data` every `E2E_POLL_INTERVAL` (500ms) regardless of an explicit wake, and
  `has_new_data` peeks `ToDeviceStore::poll_since(..., limit: 1)` (non-destructive) and
  `DeviceKeyStore::current_stream_pos()` against the baseline. The device-list check is
  deliberately not scoped to `shared_users` here (an over-broad wake just costs one harmless extra
  response-building pass; only the response actually sent enforces the privacy scope in `build`
  itself).
- **`crates/hs-user/src/state.rs`**: `UserState` gained an `e2e: Arc<dyn hs_e2e::store::E2eStore>`
  field, populated in `crates/hs-cli/src/serve.rs`'s `build_session_mounts` from the *same* `Arc`
  used to build `hs-e2e`'s own `E2eState` (opened once, shared, not opened twice over the same
  backend). `crates/hs-user/src/routes/sync.rs` passes `state.e2e` and `requester.device_id` into
  `sync::build`.
- **`crates/hs-user/Cargo.toml`**: added `hs-e2e = { path = "../hs-e2e" }` (a plain path
  dependency, matching how every other internal crate dependency in this workspace is declared --
  no `[workspace.dependencies]` entry needed or added). No cycle: `hs-e2e` does not depend on
  `hs-user`.
- **Bug found and fixed in `hs-user` (not `hs-room`): a genuine route collision with a track 04
  addition landed mid-session.** `hs-room` added its own `GET`/`POST /publicRooms`
  (`crates/hs-room/src/routes/directory.rs`) -- correctly, per
  `docs/workstreams/04-room-and-events.md`, which lists "aliases and directory" under track 04's
  ownership. `hs-user` already had its own, earlier (arguably out-of-brief) implementation of the
  same two routes (`crates/hs-user/src/routes/rooms.rs`), mounted at the same path. Mounting both
  routers together (as `hs-cli`'s `build_router` does, and as this crate's own
  `tests/sync_scenario.rs` does) panics at router-build time (`hs-http`'s `Builder` rejects an
  overlapping method+path registration) -- this took the real `hs` binary down at boot,
  confirmed live (`hs serve exited early with exit status: 101`). **Fixed** by unmounting
  `hs-user`'s two `/publicRooms` routes from `crate::routes::router` (`crates/hs-user/src/routes/mod.rs`);
  the handler code, store methods (`UserStore::list_public_rooms`) and `crate::hub`'s
  directory-entry population are left in place, unused, rather than deleted, in case track 04's
  version needs something this one already has. All of `hs-user`'s own tests pass unchanged (none
  asserted the route's mounted-ness).
- **Acceptance check, run against the real binary**
  (`cargo build -p hs-cli --bin hs && RUST_LOG=info cargo test -p hs-loadgen --test real_client_encrypted -- --nocapture`):
  both `KNOWN BUG` lines are gone, replaced by their hard-assertion success lines. Exact output:
  ```
  bob's /sync reported alice's device-list change in device_lists.changed, as it should for a user he shares a room with
  ...
  bob DECRYPTED alice's message end to end: "the wire only ever sees ciphertext for this one" came back correctly (to_device key present in the raw /sync response: true, 1 to-device event(s) delivered)
  ```
  The test binary as a whole still reports `FAILED`, but at a *later*, unrelated step (11, the
  `/keys/claim` concurrency probe): `ATOMICITY VIOLATION: at least one one-time key was handed to
  more than one of 53 concurrent /keys/claim callers`, reproducible on every rerun. This is
  entirely inside `hs-e2e`/`hs-kv` (both off limits this session): `hs-e2e`'s claim logic and
  atomicity test were untouched by this session's diff (which only added *read-only* calls --
  `count_one_time_keys`, `unused_fallback_key_algorithms` -- to the sync path, never anything on
  the claim path), and `git status` shows `hs-kv` (`conformance.rs`, `error.rs`, `lib.rs`,
  `retry.rs`, a new `postgres_backend.rs`) actively being modified by another track this same
  session. `docs/status/08-e2ee.md` records this exact atomicity guarantee as proven correct
  earlier this session ("203 concurrent claims yielded exactly 200 distinct keys with zero
  double-claims"), so this looks like a regression introduced by that concurrent `hs-kv` work,
  not a pre-existing gap. **Reported here, not fixed** (out of this track's owned files); whoever
  owns `hs-kv`/`hs-e2e` next should re-run
  `cargo test -p hs-loadgen --test real_client_encrypted -- --nocapture` once their own change
  settles.
- Confirmed `cargo test -p hs-loadgen --test real_client` (the unencrypted 17-step scenario) still
  passes unchanged, 17/17 steps including the display-name round trip (implemented by another
  track since session 1 -- no longer a known gap).
- Verification commands, all clean: `cargo fmt -p hs-user -p hs-cli`, `cargo clippy -p hs-user
  --all-targets -- -D warnings`, `cargo clippy -p hs-cli --all-targets -- -D warnings`,
  `cargo test -p hs-user` (41 unit + 2 integration tests), `cargo test -p hs-cli --test e2e` (9
  tests).

### Decisions made (session 2)

- **To-device deletion is keyed off the token a request *presents*, not eagerly right after the
  response carrying the messages is built.** See "Done" above for the full reasoning; this is a
  deliberate departure from the RFC's own suggested "delete eagerly, matching Synapse" fallback,
  made because this crate's token already carries the exact cursor needed to do better without
  extra storage.
- **`device_lists` is only computed/included on an incremental sync**, matching the spec's "only
  present on an incremental sync" rather than sending an always-empty-on-initial-sync object.
- **`device_lists.left` is built from `m.room.member` leave/ban events visible in this response's
  own timelines**, not a persisted "previously shared" snapshot. Cheap (reuses data already being
  computed) and correct for the common case (a member leaving a room the syncing user is actively
  syncing); does not catch a leave that happened entirely outside any timeline this user's syncs
  ever rendered (e.g. a very old leave replayed only via `state`, never `timeline`). Flagged as a
  reasonable Phase-0 approximation, matching the RFC's own "flagged for whoever implements this to
  settle against Synapse's behavior" allowance.
- **The long-poll's e2e wake check is not scoped to `shared_users`.** See "Done" above --
  correctness lives entirely in `build`'s actual response; the wake condition just needs to not
  miss a wakeup, and an occasional spurious one is free.
- **Removed `hs-user`'s own `/publicRooms` mount rather than `hs-room`'s.** Directory endpoints are
  explicitly track 04's per the brief; `hs-user`'s implementation predates that boundary being
  exercised. Recorded here since another track reading `crate::routes::router`'s doc comment might
  wonder why two implementations exist in the tree.

### Interfaces provided (session 2)

- `UserState<B, R>` (`crates/hs-user/src/state.rs`) now has a public `e2e: Arc<dyn
  hs_e2e::store::E2eStore>` field. Any other code composing this crate's state (only `hs-cli` does
  today) must supply it.
- `crate::sync::build`'s signature changed: now takes `e2e: &Arc<dyn hs_e2e::store::E2eStore>` as
  its second parameter, and `SyncParams` gained a `device_id: Option<OwnedDeviceId>` field. Any
  direct caller (only `crate::routes::sync::get_sync` in production; several unit tests) needs
  updating -- all in-tree call sites already are.

### Interfaces needed (session 2)

None new. The RFC's ask is now fully implemented; no further cross-track interface is required for
this specific gap.

### Shared dependencies added (session 2)

- `hs-user/Cargo.toml`: `hs-e2e = { path = "../hs-e2e" }` (ordinary path dependency, not a
  `[workspace.dependencies]` entry -- matches this workspace's existing convention for internal
  crates).

---

## Session 1's task

Build a runnable end-to-end scenario in `crates/hs-loadgen` using `matrix-rust-sdk` (real client
library, not this workspace's test helpers) driving a real, separately-running `hs serve` process
over real HTTP: register two users, log in, create a room, invite, join, send messages both ways,
sync both clients and see each other's message, set/read a display name, set/read room name and
topic, read room members, paginate `/messages`, log out. Fix what can be fixed in `hs-user` and
`hs-room`; report what can't, and who owns it.

**The scenario is written and passes end to end against the real binary.** 16 of 17 logged steps
succeeded; the one that didn't (display name) is a documented, worked-around gap in a route no
crate in this workspace mounts (see "Bugs found in other tracks"). This is the first session in
which a real Matrix client library — not this workspace's own test scaffolding — has completed a
full register/room/message/sync/logout cycle against `hs serve`.

## Done

- **`crates/hs-loadgen`** (new): a real end-to-end scenario driving `matrix-sdk` 0.19
  (`default-features = false`, `rustls-aws-lc-rs` only — no `e2e-encryption`/`sqlite`, this
  scenario needs neither and both add substantial build weight) against a real, separately
  spawned `hs serve` **subprocess** (not `hs_cli::serve::spawn_serve` in-process — deliberately a
  second OS process over a real socket, the way Element Web or any other client actually connects,
  with none of this workspace's own test scaffolding in the loop).
  - `src/harness.rs`: locates the compiled `hs` binary (`target/{debug,release}/hs`, or
    `HS_LOADGEN_BIN` to override), writes a minimal native-config YAML to a temp data directory,
    reserves a free port, spawns `hs serve -c <config>`, and polls
    `GET /_matrix/client/versions` until it answers (or the process exits early, in which case it
    surfaces the captured stderr instead of just timing out).
  - `src/scenario.rs`: the twelve-deliverable scenario (17 logged steps — some deliverables split
    into multiple asserted calls), using two `matrix-sdk` `Client`s (Alice, Bob). Every step is
    wrapped in `anyhow::Context` naming the exact HTTP call, so a failure reports which call broke
    and the server's actual response body (via `matrix-sdk`'s own `Error` Display, which includes
    the parsed Matrix error or raw body) — never a bare "assertion failed". Registration uses
    `m.login.dummy` UIA directly (the flow `hs-auth` accepts today, per
    `crates/hs-cli/tests/e2e.rs`). The sync assertions are deliberately **incremental**
    (`since=<token from a baseline sync>`), not a single full initial sync, since "`limited`,
    `prev_batch` and state-at-timeline-start semantics are where clients break" per this track's
    own brief — a full initial sync would hide exactly the bugs this scenario exists to find.
  - `tests/real_client.rs`: the runnable entry point.
  - `cargo fmt -p hs-loadgen` clean; `cargo clippy -p hs-loadgen --all-targets -- -D warnings`
    clean; `cargo test -p hs-loadgen` green (1 test, the full scenario, ~6-9s wall time).

  **Rerun it with:**
  ```
  cargo build -p hs-cli --bin hs
  cargo test -p hs-loadgen --test real_client -- --nocapture
  ```

- **Bug found and fixed in `hs-room` (#1): join responses were missing `room_id`.**
  `POST /rooms/{roomId}/join` and `POST /join/{roomIdOrAlias}`
  (`crates/hs-room/src/routes/membership.rs`) answered `{}` on every join — all actions
  (join/leave/invite/kick/ban/unban) shared one `act()` helper that always returns an empty body.
  The spec requires `{"room_id": "!..."}` from both join endpoints. `matrix-rust-sdk`'s
  `Client::join_room_by_id` deserializes the join response strictly and rejected the empty body
  outright: `Api(Deserialization(Json(Error("missing field \`room_id\`", ...))))`. No unit test in
  `crates/hs-room/tests/scenario.rs` had caught this because none of them asserted the join
  response body, only that the membership state change itself took effect — exactly the "tests
  miss it because they speak the server's own dialect" gap this exercise exists to find. **Fixed**
  by splitting out `act_join` (same membership-actor call, but responds `{"room_id": room_id}`);
  `act` is now only used by leave/forget/invite/kick/ban/unban, all of which correctly answer `{}`
  per spec. All of `hs-room`'s existing tests still pass unchanged (none had asserted the old,
  wrong body).

- **Bug found and fixed in `hs-room` (#2): empty state keys with a trailing slash 404'd.**
  `matrix-rust-sdk`'s `Room::set_name`/`Room::set_room_topic` (and the underlying
  `send_state_event`) build the state-event URL unconditionally as
  `.../state/{eventType}/{stateKey}` even when `stateKey` is empty, producing a request like
  `PUT /_matrix/client/v3/rooms/{roomId}/state/m.room.name/` — a *literal* trailing slash, not the
  same as the no-key route `/rooms/{roomId}/state/{eventType}` (no trailing slash at all), which
  `crates/hs-room/src/routes/mod.rs` already registered separately. Axum's router treats a path
  ending in `/` as a distinct route from the same path without it, and a `{stateKey}` capture does
  not match an empty segment, so this always 404'd even though `PUT .../state/m.room.name`
  (without the trailing slash) worked. This is exactly the "wrong error shape / 404 on a route real
  clients call" case this exercise was written to find, and it blocked the room-rename/topic step
  of the scenario outright the first time it was tried. **Fixed** by registering two additional
  literal routes, `PUT` and `GET /rooms/{roomId}/state/{eventType}/` (trailing slash, no capture
  after it), reusing the existing `put_state_no_key`/`get_state_no_key` handlers unchanged. All of
  `hs-room`'s existing tests still pass unchanged.

- **Clippy fixes in `hs-user` and `hs-room` unrelated to the bugs above**, needed to satisfy this
  session's mandated `cargo clippy -p hs-loadgen -p hs-user --all-targets -- -D warnings` (clippy
  lints local path dependencies too, so `hs-room`'s pre-existing violation blocked this even though
  `hs-room` wasn't in the `-p` list):
  - `crates/hs-user/src/store/tables.rs`: two `&user_id.to_string()` call sites
    (`unnecessary_to_owned`) replaced with `user_id.as_ref()`.
  - `crates/hs-user/src/hub.rs`: a test helper's return type (`type_complexity`) factored into two
    local `type` aliases (`TestRoomRegistry`, `TestHub`), test-only, no behavior change.
  - `crates/hs-room/src/actor.rs`: `RoomActor::send_event_citing` has 8 parameters
    (`too_many_arguments`, threshold 7). **Not refactored**: `hs-federation`
    (`crates/hs-federation/src/{join,inbound}.rs`, off limits this session) calls it directly with
    today's signature, and bundling the event-shape fields into the existing `pipeline::NewEvent`
    struct (the obvious fix) would change that signature out from under a crate this track cannot
    edit or verify. Silenced with `#[allow(clippy::too_many_arguments)]` and a comment explaining
    why, rather than risking an unreviewed breaking change to another track's caller.
  - `cargo test -p hs-room` / `-p hs-user`: unchanged pass counts (28+3 and 37+2 respectively)
    after all of the above.

## Bugs found in other tracks (not fixed here — out of scope, reported per instructions)

1. **(Resolved during this session by the integration lead / track 06, not by this track.)**
   `crates/hs-cli/src/federation.rs`'s `RegistryRoomSource` did not implement `room_version`,
   `forward_extremities` and `state_for_join`, added to the `RoodDataSource` trait mid-flight by
   track 06's federation work; `cargo build -p hs-cli --bin hs` failed outright with `E0046` for
   roughly the first third of this session. The integration lead confirmed this was track 06's own
   in-progress work and asked this track to wait rather than touch `hs-cli`/`hs-federation`; a
   background poll (`cargo build -p hs-cli --bin hs` retried every 20s) picked up the fix on
   attempt 17 (~5-6 minutes). Recorded here only so a future reader of this file understands why
   the scenario run has a mid-session gap, not as an open item.

2. **Missing route, no owning track claims it explicitly: `PUT`/`GET
   /_matrix/client/v3/profile/{userId}/displayname` does not exist anywhere in this workspace.**
   Confirmed by grep across every crate's `src/` and `docs/status/routes.json` (only a federation
   `query/profile` route exists, nothing under the client-server profile family). `PLAN.md`'s
   route table lists `profile/{id}` and `profile/{id}/{field}` under "WS4 Client API and auth
   (legacy)", which most naturally maps to track 07 (auth and identity, which already owns
   `/account/whoami`, devices, and the rest of the account surface) but no brief says so
   explicitly. `matrix-rust-sdk`'s `Account::set_display_name`/`get_display_name` both 404
   against this server today (confirmed live: `PUT .../profile/@loadgen-alice.../displayname` ->
   `404`, empty body). **Worked around in the scenario**: the display-name step
   (`crates/hs-loadgen/src/scenario.rs`, step 8) catches the failure, logs it as a known bug, and
   continues the rest of the scenario rather than aborting the whole run — confirmed live, the
   scenario completes all 17 steps with this one logged as `KNOWN BUG`. Not fixed here: `/profile`
   is account data, not sync/room state, and belongs in `hs-auth` (off limits this session) unless
   another track claims it first.

## What the scenario actually proved, run against the real binary

In order, from the passing run's log (`cargo test -p hs-loadgen --test real_client -- --nocapture`):

1. Registered two real users via `m.login.dummy` UIA.
2. Logged `alice` in again on a second device via `POST /login` (distinct from the session
   `register` already produced).
3. `alice` created a room (`POST /createRoom`).
4. `alice` invited `bob` (`POST /rooms/{roomId}/invite`).
5. `bob` joined by room ID alone (`POST /rooms/{roomId}/join`) — exercises bug #1's fix.
6. Both clients completed a baseline `/sync` (establishing `since` tokens).
7. Both users sent a message; each was returned a real event ID.
8. Both users' **incremental** `/sync` (with `since=<baseline token>`) saw the other's message in
   `rooms.joined[room_id].timeline.events` — the `limited`/incremental-sync path this track's brief
   calls out as the highest-risk area, and it round-tripped correctly.
9. Display name: 404'd as documented (bug #2... numbered #2 in "bugs found in other tracks";
   logged, scenario continued).
10. Room rename (`set_name`) and topic (`set_room_topic`) both succeeded once bug #2 (the
    trailing-slash route) was fixed, and the resulting `m.room.name`/`m.room.topic` events were
    confirmed present in a subsequent `/sync`'s timeline.
11. `GET /rooms/{roomId}/members` listed both users.
12. `GET /rooms/{roomId}/messages` (backward pagination) returned alice's message in the page.
13. Both clients logged out (`POST /logout`), and a `/sync` call with alice's now-invalidated token
    was confirmed rejected with `401 M_UNKNOWN_TOKEN` — logout actually revokes the token, not just
    a client-side no-op.

## Next

1. `crates/hs-loadgen`'s scenario currently proves the client-server basics; it does not exercise
   sliding sync (MSC4186) at all — `matrix-sdk`'s `sync_once` here uses the legacy `/sync` path.
   The brief's definition of done wants Element X's sliding-sync path green too; that needs
   `matrix_sdk::SlidingSync` client code once `hs-user`'s sliding-sync endpoint exists.
2. `matrix-sdk`'s `sync_once` always sends `use_state_after: true` (MSC4222) on every `/sync`
   request. `hs-user`'s `SyncQuery` (`crates/hs-user/src/routes/sync.rs`) has no
   `use_state_after` field, so the parameter is silently ignored by axum's `Query` extractor
   (unknown fields aren't rejected) rather than erroring — safe for this scenario (a small,
   non-`limited` incremental sync puts everything in `timeline.events` regardless of `state_after`
   semantics, confirmed by step 10 above passing), but a real MSC4222 implementation is still owed
   before sliding-sync work claims done, since a `limited: true` response is exactly where the two
   semantics diverge and this scenario never produced one.
3. If `/profile` gets claimed and implemented by another track, remove the `match`/soft-fail
   wrapper around step 8 in `crates/hs-loadgen/src/scenario.rs` and assert it hard like every other
   step.

## Interfaces provided

Unchanged this session except the two bug fixes above (`POST /rooms/{roomId}/join`,
`POST /join/{roomIdOrAlias}`, and the trailing-slash state-event routes), which are fixes to
existing interfaces, not new ones.

## Interfaces needed

Nothing new. (Session was blocked for several minutes on `hs-cli` building at all — see "Bugs
found in other tracks" #1 — but that resolved without this track's intervention.)

## Decisions made

- **`hs-loadgen` spawns a real OS subprocess, not `hs_cli::serve::spawn_serve` in-process.**
  `crates/hs-cli/tests/e2e.rs` already proves the in-process router works; the point of this crate
  is the thing that in-process test cannot prove — a real client over a real socket against an
  independently-running process, closer to what Element Web (the next step per
  `docs/next-steps.md`) will do. Recorded here since another track reading this file might
  otherwise expect `hs-loadgen` to depend on `hs-cli`; it does not, and should not need to.
- Registration in the scenario uses `m.login.dummy` UIA in a single request (no UIAA
  probe-then-retry loop), matching what `crates/hs-cli/tests/e2e.rs` already proved this server
  accepts.
- Left `RoomActor::send_event_citing`'s 8-argument signature alone (silenced the clippy lint
  instead of refactoring) because `hs-federation` calls it directly and is off limits this
  session; see "Done" above.

## Shared dependencies added

- `matrix-sdk = "0.19"` (root `Cargo.toml` `[workspace.dependencies]`), `default-features = false`,
  features = `["rustls-aws-lc-rs"]`. Named for exactly this purpose in
  `docs/workstreams/14-test-and-conformance.md`. Pulls in its own independent `ruma` 0.17 (via
  transitive deps), unrelated to and not unified with this workspace's own `ruma` 0.15 — expected,
  since `matrix-sdk` vendors its own client-API types and this crate never mixes the two.
- `reqwest` (already a workspace dependency, added by track 15): used directly in
  `crates/hs-loadgen/src/harness.rs` for the readiness poll, ahead of any `matrix-sdk` client
  existing.
