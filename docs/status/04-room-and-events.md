# 04 Room and events: status

Track brief: `docs/workstreams/04-room-and-events.md`. Owner crate: `hs-room`.

Last updated: 2026-09-25 (session 8: RFC 0015 implemented -- a room this server's own user
joined on another server is built here from the verified `send_join` response, as outliers plus a
join with explicit state; `RoomRegistry::bootstrap_from_remote_join` is the entry point `hs-cli`
should call).

> **Integration note, 2026-09-25 (integration lead):** the caller RFC 0015 needed exists.
> `crate::remote_join::RemoteJoin` is a hook on `RoomState` (like the registry's fencing and
> token-resolver hooks: installed by `hs-cli`, `None` in this crate's tests), and
> `crate::routes::membership`'s two join handlers fall through to it when `get_or_load` answers
> `RoomNotFound`: the client's `server_name`/`via` query parameters (repeated keys, read from the
> raw query) plus the room ID's own server as a last resort, and a remote alias resolved through
> its server's directory. `RoomError::RemoteJoinFailed` (`502 M_UNKNOWN`) is what a join that no
> server could sponsor answers with; a resident's `403` is `Forbidden`, its `404` is
> `RoomNotFound`. Tests in that module; end to end in `crates/hs-cli/tests/federation_two_servers.rs`.

## Session 8 (2026-09-25): RFC 0015 -- a room this server's own user joined elsewhere now exists here

**Assignment**: implement `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md` in this
crate (plus one additive method in `hs-state`): build a `RoomActor` from a verified federation
`send_join` response, so that after `hs_federation::outbound_join::join_room` completes, the
joining user's own server can represent the room -- read its state, list its members, post into
it, accept the resident's next events, sync it. Ownership this session: `crates/hs-room/**`,
`crates/hs-state/src/kv_store.rs` (one additive method, by instruction), this file, the RFC's
status line. No git.

### What was built

**The entry points** (`crates/hs-room/src/actor.rs`, `crates/hs-room/src/registry.rs`):

- `RoomActor::accept_remote_join_with_state(&mut self, state: Vec<Event>, auth_chain: Vec<Event>,
  join_event: Event) -> Result<RemoteEventOutcome, RoomError>` -- the core. Works on an empty
  shell (a brand-new room) and on a room that already exists here (a user who left and rejoins,
  or a second local user joining a room the first already brought over) alike. A join already
  held answers `RemoteEventOutcome::AlreadyKnown` and touches nothing.
- `RoomActor::create_from_remote_join(backend, tables, identity, room_id: &RoomId, room_version:
  RoomVersionId, state, auth_chain, join_event) -> Result<Self, RoomError>` -- an empty shell
  (`RoomActor::empty_for`, crate-private) with the above applied. The `now_ms` the RFC sketched is
  gone: nothing is built or timestamped here, every event arrives already made.
- `RoomActorHandle::accept_remote_join_with_state(...)` -- the async, `spawn_blocking` wrapper,
  same shape as `accept_remote_event`.
- `RoomRegistry::bootstrap_from_remote_join(&self, room_id, room_version, state, auth_chain,
  join_event) -> Result<RoomActorHandle<B>, RoomError>` -- what `hs-cli` should call. If
  `get_or_load` finds the room, the response is applied to the existing actor; otherwise an empty
  shell is registered *first* (new crate-private `RoomRegistry::insert_if_absent`, which never
  displaces an entry a concurrent caller registered in between) and the join is applied through
  its handle, so the join's `RoomUpdate` -- with `membership_deltas: [(user, "join")]`, exactly as
  `persist` publishes for any membership event, which is what `hs-user`'s session hub keys off --
  is published from an actor that is already resident and already on the global stream. Applying
  the join before registering would publish from an actor nobody could look up, and a consumer
  reacting to it would load a second copy of the room from disk. A shell whose join is refused is
  dropped again (`RoomRegistry::drop_if_unbootstrapped`); nothing durable records it, because
  `Tables::room_meta` is only written with the join.

**What the snapshot becomes: outliers.** `state ∪ auth_chain`, deduplicated by event ID and less
anything already held, is persisted by the new `RoomActor::persist_outliers` with
`hs_model::event::EventFlags::OUTLIER` set (the flag existed, unused, since track 02 defined it),
`PersistedEvent.room_pos: None`, no timeline entry, no forward-extremity entry, no `RoomUpdate`,
in `hs-kv` transactions of up to 256 events each with the cluster-fencing check in every one.
They *are* written to `events`/`event_sn`, so `event_by_id`, `state_at_event`, `/state`,
`/members`, `/event/{id}` and every auth check can see them, but `/messages`, `/sync` timelines,
`events_after` and `paginate` never do. They are persisted and fed to the state store in
topological order (`depth`, then `origin_server_ts`, then event ID -- `crate::actor::topological_order`)
so each one's `auth_events` are indexed before it is, which is what lets the chain-cover index and
any later state resolution see the snapshot's auth chains whole. **An outlier's own `state_at` in
the state store is meaningless** -- it is fed with *no* prev events (`RoomActor::feed_store_outlier`),
because its real prev events are mostly events this server has never seen and resolving the ones
it happens to hold would run state resolution over a fragment -- and nothing may read it as the
room's state at that point. The one read path that used to: `RoomActor::event_visible_to`, which
evaluates history visibility against `state_at(event)`, now answers for an outlier by
`can_see_current_membership(requester)` (currently joined, or the room is world-readable), since
a snapshot event is a piece of the room's state as the join found it and has no position in
anybody's history here.

**The join is the room's first timeline event** (`room_pos` 1 for a new room), the **sole forward
extremity**, with its state set **explicitly** to `state` + itself. `crates/hs-state/src/kv_store.rs`
gained `KvStateStore::add_event_with_state(..same twelve parameters as add_event.., state: &[EventSn])`:
records the event exactly as `add_event` does, but builds the state before it by applying a
`StateDiff` of every snapshot event's interned `(type, state_key) -> EventSn` over
`empty_root()` instead of resolving prev events, then applies the event's own key, stores
`state_at`, and updates the chain index. Refuses (`KvStoreError::UnknownEvent`, recording nothing)
an entry it has not ingested. The `StateStore` trait is unchanged. (`add_event`'s body was split
into `record_event`/`apply_and_index` so the two share everything but the middle;
`InMemoryStateStore` was deliberately *not* given a mirror -- nothing in this crate constructs
one against a snapshot, and the reference store's "stays hand-written and small" charter in its
module docs argued against growing it for one production caller.) The join's unknown
`prev_events` are simply not resolved -- not `MissingAncestors`, which stays the answer for an
ordinary `accept_remote_event` -- and `old_extremities` for the join is *every* current forward
extremity, not only the prev events it names (there are none held): see "Decisions made".

**Durable, and reloaded identically.** Two new keyspaces in `crates/hs-room/src/persist.rs`:

- `Tables::outliers`, keyspace `room_outliers`, `(RoomSn, EventSn) -> b""` -- the room's outliers,
  scanned by `RoomActor::load` before the timeline.
- `Tables::state_snapshots`, keyspace `room_state_snapshots`, `(RoomSn, EventSn) -> bytes` -- the
  explicit state a timeline event was fed with, as concatenated 8-byte big-endian `EventSn`s
  (`persist::encode_event_sns`/`decode_event_sns`). Written in the join's own transaction
  (`RoomActor::persist_with(event, PersistKind::RemoteJoin { snapshot })`; `persist` is now a thin
  `PersistKind::Ordinary` wrapper).

`RoomActor::load` now: reads every outlier row, decodes each event, re-sorts topologically (rather
than trusting key order alone, in case a snapshot event's `EventSn` was interned early as a
relation target), absorbs each as an outlier; reads every `state_snapshots` row; then replays the
timeline as before, feeding an event with a snapshot row via `add_event_with_state`. **Side fix,
found on the way**: `load` never restored `PersistedEvent.flags` onto the re-parsed `Event`
(`Event::parse` starts every event with no flags), so a redaction's `REDACTED` flag -- which
`crate::routes::render::readable_json` checks at render time -- was lost across an eviction and
reload, and a redacted event would have rendered unredacted afterwards. Flags are restored for
every loaded event now; the outlier flag needed the same line.

`RoomActor::persist`'s "write `room_meta` on the first event" test changed from `events.is_empty()`
to `timeline.is_empty()`: identical before outliers existed (the two sets were the same), and
now correct when outliers are persisted first -- a room whose only records are outliers is not a
room `load` may find, so a bootstrap that dies between the outliers and the join leaves nothing
visible and the retry starts clean (re-interning the same IDs, overwriting the same records).

### The trust decision

`accept_remote_join_with_state` does **not** re-verify hashes or signatures (the caller,
`hs_federation::outbound_join::join_room`, already ran `verify_pdu` over every event), does
**not** run state resolution over the snapshot (it is the resident's already-resolved state, by
construction), and does **not** auth-check the snapshot's own events (they *are* the auth chain
everything else is judged against; re-deriving them would mean fetching the room's whole history,
which is exactly what a `send_join` response exists to make unnecessary). This is the same trust
decision every homeserver makes for a `send_join` response, Synapse included (read for behavior
only, per this track's brief): trust the resident's state, verify its signatures, and authorize
what you add on top. What *is* checked, before anything is written (`RoomActor::validate_remote_join`):
every event is for this room and this room's version; every snapshot event is a state event;
`state` holds exactly one `m.room.create` with an empty state key and no two events for one
`(type, state_key)`; the join is an `m.room.member` with `membership: join`, `state_key == sender`,
and a sender on `identity.server_name`; every ID in the join's `auth_events` is in the snapshot;
and `hs_state::auth::check_auth_events_selection` then `check_event_auth` accept the join against
both the state its `auth_events` imply and the full `state` (the two hard checks
`accept_remote_event` already runs), so a resident cannot hand this server a join its own rules
would reject. Shape problems are `RoomError::InvalidEvent(EventError::Format(..))`; the wrong
server, an `auth_events` entry outside the snapshot, and an auth failure are `RoomError::Forbidden`,
each with a message that says what was wrong.

### Tests

`crates/hs-room/tests/remote_join.rs` (7 tests, public API only): two backends stand in for two
servers. `a.example` hosts the room via `RoomActor::create_room` (alice, `public_chat`, room
version 11), alice sends a message, `@bob:b.example` joins through `membership_action` (the
resident signs it with `a.example`'s key -- this crate never verifies signatures, and the test
says so); the response is `state_at_event(<alice's message>)`'s `state` and `auth_chain` plus
bob's join, whose `prev_events` is alice's message, which the snapshot does not carry.

- `a_room_joined_elsewhere_is_bootstrapped_from_the_resident_snapshot`: on `b.example`,
  `create_from_remote_join`; `full_state()` holds create, power levels, join rules, history
  visibility, alice's and bob's member events; `joined_members()` has both; the timeline is
  exactly the join (`events_after(0, 100).len() == 1`, `paginate` backward yields only the join);
  alice's pre-join message is *not* held; `event_by_id(create)` is Some, flagged outlier, and
  visible to bob; `state_at_event(join)` is the snapshot plus the join; bob's `send_event` on B
  cites the join as its only prev; alice's next message on A (citing bob's join) is accepted on B
  via `accept_remote_event`; `RoomActor::load` over B's backend reproduces `full_state()`, the
  timeline, `state_at_event(join)` and the outlier flag; after the reload bob leaves via
  `membership_action`.
- `applying_the_same_join_response_twice_is_a_no_op`: `AlreadyKnown`, one timeline entry.
- `a_join_by_a_user_of_another_server_is_refused` (`Forbidden`),
  `a_snapshot_without_a_create_event_is_refused` (`InvalidEvent`),
  `a_join_whose_auth_events_leave_the_snapshot_is_refused` (`Forbidden`; asserts first that the
  join really does cite the join-rules event it then removes).
- `bootstrap_from_remote_join_creates_the_room_and_announces_the_join_once`: an unknown room is
  created through the registry; a `subscribe_global()` subscriber receives the join's
  `RoomUpdate` with `room_pos == 1`, `global_seq >= 1` and bob's `membership_deltas`; the same
  response again stores nothing and publishes nothing; `get_or_load` then finds a real room.
- `a_refused_bootstrap_leaves_no_room_behind`: a refused join leaves `resident_count() == 0` and
  `get_or_load` answering `RoomNotFound`.

`crates/hs-state/src/kv_store.rs::tests::add_event_with_state_seeds_the_state_from_an_explicit_snapshot`:
three outliers with no prev events, then a join with an unknown prev and the three as explicit
state -- `state_at(join)` is exactly the four, the chain index reaches the create through power
levels, and an unknown snapshot entry is `UnknownEvent` that records nothing.

Mutation-checked by hand and reverted: with `load` skipping outliers, and separately with the join
fed via plain `feed_store` instead of `feed_store_with_state`, the end-to-end test fails.

### How to verify (session 8)

```
cargo fmt --all --check
cargo clippy -p hs-room -p hs-state --all-targets -- -D warnings
cargo test -p hs-room -p hs-state
cargo build --workspace
```

`cargo test -p hs-room`: 70 unit + 7 `remote_join` + 12 `scenario` (was 70 + 12). `cargo test -p
hs-state --lib`: 71 (was 70). All four commands were run clean at the end of the session.

### Interfaces provided (new this session)

- `hs_room::actor::RoomActor::{accept_remote_join_with_state, create_from_remote_join}`,
  `hs_room::actor::RoomActorHandle::accept_remote_join_with_state`,
  `hs_room::registry::RoomRegistry::bootstrap_from_remote_join` -- see "What was built".
- `hs_room::persist::{OutlierKey, StateSnapshotKey, encode_event_sns, decode_event_sns}` and the
  two new `Tables` fields (`outliers`, `state_snapshots`). `Tables` has public fields; nothing
  outside this crate constructs it by struct literal (checked), so this is additive.
- `hs_state::kv_store::KvStateStore::add_event_with_state` -- additive; `StateStore` unchanged.

### Interfaces needed

- **06 (federation) / `hs-cli`** -- the wiring this crate cannot do from here: after
  `hs_federation::outbound_join::join_room` returns its `RemoteJoinOutcome { room_id, room_version,
  join_event, state, auth_chain, .. }`, call
  `registry.bootstrap_from_remote_join(&room_id, room_version, state, auth_chain, join_event).await`
  on the `Arc<RoomRegistry<B>>` `hs serve` already holds. That is the whole integration.
  `crates/hs-cli/src/federation.rs::run_join_room` (the `hs federation-join-room` command) opens
  no storage by design and stays a diagnostic of the handshake alone (it says so now); the
  client-server `/join` route for a room ID this server does not host (with `via` /
  `server_name` hints) is where this lives in `hs serve` -- see the integration note at the top. The RFC's suggested
  `RoomWriteSink` extension is track 06's call; this crate's handle method is what it would call.
- **06 (federation)** -- backfill. Nothing before the join is in the timeline; `/messages`
  backward from the join stops at the join. `Tables::extremities_bwd` and negative `room_pos`
  are still the intended shape for what backfill writes.

### Decisions made this session

- **A remote join supersedes every forward extremity this actor held**, not only the (unknown)
  prev events it names. Matters only on the rejoin path: the old extremities are this server's
  own last events from before its user left (its leave, typically), which the resident's resolved
  state already accounts for; keeping them would make the room a permanent fork between stale
  local state and the resident's snapshot, resolved on every read. `PersistKind::RemoteJoin`'s
  doc comment records it.
- **Outliers are fed to the state store with no prev events at all**, rather than with whichever
  prev events happen to be held: deterministic, no resolution over fragments, and it makes "an
  outlier's `state_at` is meaningless" a property rather than a matter of luck. `auth_events`
  *are* resolved, for the chain-cover index and for state resolution's benefit.
- **`Tables::room_meta` is written with the room's first *timeline* event**, never with outliers,
  so a half-finished bootstrap is invisible to `load` and a retry starts clean.
- **`event_visible_to` on an outlier answers `can_see_current_membership`** rather than the
  history-visibility algorithm over a state that does not exist here; recorded as the one
  deliberate consumer-side accommodation of meaningless outlier `state_at`.
- **`load` restores persisted flags onto every event** (side fix; see "What was built").
- **`InMemoryStateStore` did not get an `add_event_with_state` mirror** -- see above.

### Not done (and not started)

- **No backfill of the pre-join timeline** -- track 06's `/backfill` client is where that lands;
  this crate has the tables for it.
- **Outliers' own `state_at` is meaningless** by design; every consumer that could read it has
  been redirected (`event_visible_to`) or never reads it (timeline reads). A future "state at an
  outlier" question -- `/context` on a snapshot event, say -- should go through the current state
  or the join's explicit state, not the store.
- **Soft failure is still absent**: `accept_remote_event`'s third check (current state at receipt
  time) is still not implemented, and every rejection is still a hard rejection, exactly as
  session 3 recorded.
- **Faster joins** (`members_omitted: true` responses) are not handled: `RemoteJoinOutcome`
  documents the resident side never omits members, and this crate would accept such a response
  as if it were complete. Worth a guard in the caller until partial state is a real feature.

## Session 7 (2026-09-19): the admin room directory, `/context` state pinning, cluster fencing

**Assignment, in priority order**: (1) implement `hs_admin::sources::RoomDirectory` over this
crate, which was left as a real `503` because nothing implemented it; (2) check the Element Web
and federation sessions' diagnoses (`docs/status/16-management-web-interface.md`,
`docs/status/06-federation.md`) for anything attributed to rooms/state/membership/events; (3) this
crate's own recorded gaps (alias validation, `/state`/`/joined_members` shape, `/context`'s
live-state bug); (4) `RoomActor::persist` never calling `Fence::check`. Ownership this session:
`crates/hs-room/**` and this file only -- no Docker, no git.

### 1. The admin room directory (`hs_admin::sources::RoomDirectory`), implemented for real

**New module `crates/hs-room/src/admin.rs`**: `RoomRegistryDirectory<B>` wraps an
`Arc<RoomRegistry<B>>` and implements every method on the trait track 15 defined and froze on
`crates/hs-admin/src/sources.rs` last session (`get_room`, `list_rooms`, `set_blocked`,
`make_admin`). `hs-admin` was added as a normal path dependency of this crate
(`crates/hs-room/Cargo.toml`) -- no cycle, since `hs-admin` depends on neither `hs-room` nor
`hs-auth`.

**Enumerating every room the server has ever created** (`GET /rooms` with no filter needs this;
`RoomRegistry` previously only knew about rooms it had loaded or created in this process, or
published ones -- the same gap session 5 named for `/search`). New:
`crate::actor::list_all_room_ids`/`RoomRegistry::list_all_room_ids`, a full scan of
`Tables::room_meta` (written once per room, at its first persisted event, and never removed) --
decodes each row's own `room_id` field directly, no interning-table round trip needed. Same
scaling caveat as `list_published_room_ids`: fine for an admin listing, not a hot path. Loading
every room's actor to build its summary (`list_rooms` calls `get_or_load` per room) also means
every room briefly becomes resident in the registry's in-process cache until the idle sweeper
evicts it -- acceptable for an admin operation, recorded here as a known scaling limit rather than
solved (a server with a very large room count would want a real summary index instead).

**`RoomActor::admin_summary`** builds the `hs_admin::model::AdminRoom` response shape directly from
this actor's existing state-reading primitives (`state_event`, `joined_members`, `full_state`,
`creation_type`) -- no new state-reading machinery, just assembly. One deliberate simplification,
called out in both the method's doc comment and here: `forgotten` is reported as
"zero currently-joined members," not "every local member who was ever here has called `/forget`" --
this crate has no durable per-user forget index across every past member (only `RoomActor::forget`'s
own in-memory, resident-lifetime-only set), and building one was out of scope for this session.

**Blocking now has a real effect**, per the trait's contract ("expected to have a real effect on
the room ... enforcing it is `hs-room`'s job"). New durable index: `Tables::blocked_rooms`
(`(RoomSn,) -> RoomBlock{reason}`, presence means blocked -- same shape as the existing
`public_rooms`/`joined_rooms` tables), plus `crate::actor::set_room_blocked`/`room_block_reason`
(free functions, room-ID-keyed, mirroring `set_directory_visibility`/`is_directory_public`'s own
shape) and a new precheck at the top of `RoomActor::send_event_citing` -- the function both
`send_event` and `membership_action` funnel through -- that reads this same table fresh on every
call (no cache to invalidate: a room already resident in memory when it gets blocked rejects its
very next send). New `RoomError::RoomBlocked(Option<String>)` variant, mapped to `403 M_FORBIDDEN`
carrying the administrator's reason. **Decision**: the block gate is unconditional, including for
`make_admin`'s own power-levels write -- an operator who wants to grant an admin in a blocked room
must unblock first. Not explicitly required by the contract either way; chosen for simplicity and
because "blocked means no local writes, full stop" is the easier invariant to reason about.

**`make_admin` sends a real `m.room.power_levels` event**, not a flag flip, per the contract's
explicit requirement. Mirrors Synapse's `make_room_admin` behaviorally (read for behavior only,
never copied, per this track's brief): the target user is very likely the one *without* enough
power yet, so the event cannot be sent with them as its own sender. `RoomActor::make_admin` grants
`min(highest power level currently held by any user in `m.room.power_levels`'s `users` map, 100)`,
sent as whichever currently-joined member already holds at least `required_power("m.room.power_levels",
true)` (ties broken by the smaller user ID, for a deterministic choice in tests). Returns
`RoomError::Forbidden` if the target is not currently joined, `RoomError::BadRequest` if no
sufficiently-privileged member is currently joined at all (a room whose only admins have all left
cannot be granted a new one this way).

**Tests**: `crates/hs-room/src/admin.rs` gained its own module (9 tests) -- `get_room` for an
unknown and a real room (including that `public`/`joined_members_count`/`creator`/`federatable`
are all populated correctly), `list_rooms` finding every room the server has ever created *after*
evicting them from residency (proving it does not depend on the in-process cache), the `blocked`
filter, `set_blocked`'s real effect (a blocked room rejects a new local event; unblocking restores
it) and its `NotFound` on an unknown room, and `make_admin` both succeeding (asserting the real
`m.room.power_levels` event's `users` map) and refusing a non-member. `cargo test -p hs-room --lib`:
58 passed before `/context` and fencing tests below, 63 after those, from 58 at session start.

**The one line `hs-cli` needs** (this track does not own `hs-cli`; noted here per this session's
explicit instruction rather than made): in `crates/hs-cli/src/serve.rs`'s `admin_state` function
(around line 536, the same one that already calls `.with_users(...)`), add a `rooms: &Arc<hs_room::
registry::RoomRegistry<B>>` parameter and:
```rust
.with_rooms(Arc::new(hs_room::admin::RoomRegistryDirectory::new(rooms.clone())))
```
and at `admin_state`'s one call site (around line 943, inside `spawn_serve`, where `rooms` -- the
`Arc<RoomRegistry<B>>` -- is already in scope from line 837, built before `admin_state` is called)
pass `&rooms` as the new argument. `AdminState::with_rooms` already exists and takes exactly
`Arc<dyn RoomDirectory>` (`crates/hs-admin/src/router.rs`); `RoomRegistryDirectory<B>` implements
that trait for any `B: KvBackend + 'static`, matching the same `B` `serve.rs` already threads
through `RoomState`/`UserState`.

### 2. What the Element Web and federation sessions found -- nothing attributed to this crate

Read both `docs/status/16-management-web-interface.md` ("pointing Element Web at `hs serve`") and
`docs/status/06-federation.md` (still at its "sixth session," TLS/CA) in full. Three real bugs were
diagnosed live against a real client in the web-interface session -- no CORS headers at all on the
client-server API, `GET /capabilities` reporting `m.set_displayname`/`m.set_avatar_url` as stale
`false`s, and `/rooms/{roomId}/receipt/...`/`.../read_markers` 404ing live despite being registered
-- and all three are explicitly diagnosed to `hs-cli`, `hs-http`'s router composition, or
`hs-user`'s routes, with `hs-room` explicitly ruled out for the third ("not a naming collision with
`hs-room`'s router... no other crate defines anything under `/rooms/{roomId}/receipt`"). Confirmed
independently: `cargo test -p hs-loadgen --test real_client -- --nocapture` (run this session, see
"Proof" below) shows receipts and read markers working end to end (`"alice's /sync saw bob's public
read receipt"`, `"bob's /sync reported his own m.fully_read marker"`), so whatever the Element
session's router-composition bug is, it is either config-specific to that session's `hs serve`
invocation or has since been fixed elsewhere -- worth flagging to whoever owns that file next, not
something this session could reproduce or fix from within `hs-room`. The federation session's own
"Next" item 0 (outbound signing over the unredacted form) was already closed by this track's
session 5 (`docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md`) -- that federation status
file just has not been updated to say so yet; nothing new to do here.

### 3. `/context`'s `state` field, pinned to the target event

**The bug, exactly as recorded**: `crate::routes::query::get_context` built its `state` field from
`actor.full_state()` -- this room's *live* current state -- instead of the state as of immediately
after the target event, the same "read a live value instead of one pinned to a point in time" bug
class already closed for `/messages`/`/event`/`/state`/`/members`. **The fix**: one call swapped,
`actor.state_at_event(target.event_id())?.state` in place of `actor.full_state()` --
`RoomActor::state_at_event` already existed and does exactly this ("the room's state immediately
after the queried event"), unused by this endpoint until now.

**Test**: `crate::routes::query::tests::context_state_is_pinned_to_the_target_event_not_current_state`
-- sends a message while the room's topic is "first", changes the topic to "second" afterwards,
and asserts `/context` for that message still reports "first" in its `state` array (with an
explicit sanity check that current state really did move on to "second" first, so the assertion is
meaningful). Fails without the fix, passes with it.

**Not reached this session** (still open, from session 5's list): alias/canonical-alias
validation, and the `/state`/`/joined_members` response-shape gaps -- session 4/5's writeup did not
leave a precise-enough description of what exactly is wrong with either shape to fix blind this
session, and neither showed up in either of the other two tracks' live-testing sessions this time.
Left for a session that can name the exact gap (or hits it live) to fix.

### 4. `RoomActor::persist` now calls `Fence::check` (the belt-and-braces cluster guard)

**The gap, exactly as track 03 recorded it**: the routing gate `hs-cli`'s `RoomShardGate` added
(session before this one, `docs/status/03-cluster.md`) is the *primary* defense against the
two-replica split-brain that file's history describes -- it stops a non-owning replica from ever
constructing a `RoomActor` at all. `RoomActor::persist` itself never checked `hs_cluster::Fence`,
which is the secondary, belt-and-braces defense for the narrow window where a real ownership
handoff (a partition, a rolling update) happens *between* the gate's check and this transaction's
commit.

**New**: `hs-cluster` added as a normal dependency of this crate (`crates/hs-room/Cargo.toml`; no
cycle, `hs-cluster` depends only on `hs-kv`). New module `crates/hs-room/src/fencing.rs`:
`RoomFencing<B>` bundles everything `persist` needs to check its fence -- `ownership: Arc<dyn
hs_cluster::Ownership>` (read fresh on every call, never cached), `layout: hs_cluster::ShardLayout`
(to compute which shard a room ID hashes to), and `cluster_store: hs_cluster::store::ClusterStore<B>`
(the keyspace `Fence::check` reads its epoch row from, opened over the exact same backend as the
room registry). `RoomActor` gained an `Option<Arc<RoomFencing<B>>>` field, defaulting to `None` on
every construction path today (both `create`'s and `load`'s struct literals) -- `None` means
`persist` behaves exactly as before this session, byte-for-byte, until something installs a
fencing hook. New `RoomRegistry::install_fencing` (same `OnceLock`, idempotent-past-first-call
shape as `install_global_token_resolver`) propagates the installed hook onto every actor the
registry constructs or loads, in both `get_or_load` and `insert` (which `create_room` also goes
through).

**Where the check runs, and why it has to be there**: as the last read inside `persist`'s existing
`transact(...)` closure (`crates/hs-room/src/actor.rs`, the same block session 3's cluster
experiment cited by line number), immediately before the closure returns `Ok`. It must be inside
this same transaction, not before it: `hs_kv`'s serializable snapshot isolation is what actually
catches a concurrent handoff -- reading the epoch row *inside* the transaction adds it to that
transaction's read set, so a concurrent `acquire_shard` write to that same row either fails this
check immediately (if it already landed) or conflicts this transaction's commit (if it lands
in between), per `hs_cluster::Fence::check`'s own contract. **How the failure gets back out**:
`transact`'s closure is fixed by `hs-kv`'s own API to return `Result<T, hs_kv::KvError>`, which has
no "fenced" variant -- the fix uses `hs_kv::KvError::Aborted` (an existing variant documented for
exactly this: "a closure ... asked for the transaction to be aborted ... carrying an
application-level error") to stop `transact`'s retry loop immediately, and a `std::cell::Cell<Option<String>>`
declared just outside the closure to carry the human-readable message back out, since `Aborted`
only carries a boxed `std::error::Error`. New `RoomError::Fenced(String)` variant, mapped to `503`
(the caller should retry against whichever replica now actually owns the shard, which is
`hs-cli`'s forwarding layer's job, not a client-visible `403`). `ownership.fence(shard)` returning
`None` (this replica's own local view no longer includes the shard at all) is treated identically
to a failed epoch check -- also a rejection, not a silent pass.

**Tests**: `crates/hs-room/src/fencing.rs` gained its own module (4 tests), using a small in-crate
fake `Ownership` (`FixedFence`, answering `fence()` with one fixed value regardless of which shard
is asked about) rather than the full async `KvOwnership` acquisition machinery, plus a real
`hs_cluster::store::ClusterStore` over a shared `MemoryBackend` for the actual epoch row:
`no_fencing_installed_is_a_no_op` (unchanged behavior with the field at its default), `a_current_fence_allows_persist`,
**`a_stale_fence_after_a_real_handoff_rejects_the_write`** (acquires a shard, then forces a second
replica to take it over -- a real epoch bump through the real store -- and asserts a send still
using the first, now-stale fence comes back `RoomError::Fenced`), and
`ownership_reporting_no_fence_at_all_rejects_the_write`.

**The one line `hs-cli` needs** (not written here, out of this track's ownership): after `let
cluster_handles = crate::cluster::start(&config, backend.clone()).await?;` in `crates/hs-cli/src/serve.rs`
(around line 959, by which point `rooms` -- the `Arc<RoomRegistry<B>>` -- has already been built at
line 837 and `backend` is still in scope), add:
```rust
rooms.install_fencing(Arc::new(hs_room::fencing::RoomFencing {
    ownership: cluster_handles.cluster.ownership().clone(),
    layout: cluster_handles.layout,
    cluster_store: hs_cluster::store::ClusterStore::open(backend.clone())
        .map_err(|e| /* whichever ServeError variant wraps a cluster-store open failure */)?,
}));
```
This is safe to call unconditionally, including in single-node mode: `Cluster::ownership()` there
returns `hs_cluster::ownership::SingleNode`, whose `fence()` always answers `Fence::inert(shard)`,
and `Fence::check` on an inert fence (`epoch: None`) is always `Ok(())` -- so installing this in
single-node mode changes nothing observable, and clustered mode gets the real check.

### How to verify (session 7)

```
cargo fmt -p hs-room
cargo clippy -p hs-room --all-targets --no-deps -- -D warnings
cargo test -p hs-room --lib            # 63 passed (up from 58)
cargo test -p hs-room --test scenario  # 11 passed, unchanged
cargo build -p hs-cli --bin hs
cargo test -p hs-loadgen --test real_client -- --nocapture   # 28 steps, all pass, receipts/read-markers included
```
`cargo test -p hs-loadgen --test real_client_encrypted` was not re-run this session (nothing this
session touched e2ee call paths); no reason to expect a regression there.

### Interfaces provided (new this session)

- **`hs_room::admin::RoomRegistryDirectory<B>`**: implements `hs_admin::sources::RoomDirectory`.
  The seam track 15 asked for; needs the one `hs-cli` line above to actually reach a running
  server.
- **`hs_room::actor::{list_all_room_ids, set_room_blocked, room_block_reason}` /
  `RoomRegistry::{list_all_room_ids, set_room_blocked, room_block_reason}`**: every room this
  server has ever created, and the blocked-room flag/reason, both room-ID-keyed and not requiring
  the room's actor to be resident.
- **`hs_room::fencing::RoomFencing<B>`** and **`RoomRegistry::install_fencing`**: the cluster-fencing
  hook, unused by anything in this crate's own wiring until `hs-cli` installs one.
- **`RoomError::RoomBlocked(Option<String>)`** and **`RoomError::Fenced(String)`**: new error
  variants, `403`/`503` respectively.

### Interfaces needed

- 15 (admin API): nothing new -- the trait contract from last session was implemented as
  specified, no changes requested back.
- 03 (cluster) / whoever next holds `hs-cli`: the two wiring lines above (admin room directory,
  cluster fencing) are the only things standing between this session's work and a running server
  actually using either.
- Unchanged from session 5: the pagination-token format mismatch between `hs-user` and this crate,
  `forgotten`/directory-publish query surfaces for `hs-user`.

### Decisions made this session

- **`hs-admin` and `hs-cluster` were added as normal path dependencies of `hs-room`** (not behind
  an indirection trait the way `hs-auth`'s `DeviceListChangeNotifier`/`GlobalTokenResolver` hooks
  were, in earlier sessions): both are cycle-free (neither depends on `hs-room` or `hs-auth`), so
  there was no reason to avoid a direct dependency the way those two cases had to.
- **The admin room directory loads every room's actor to build `list_rooms`**, with no bound on how
  many rooms end up briefly resident. Accepted as an admin-only-operation scaling limit, the same
  class of gap `/search` already carries, not fixed this session.
- **`admin_summary`'s `forgotten` field means "zero currently-joined members," not "every past
  member has explicitly forgotten it"** -- this crate has no durable index for the latter across
  every user who has ever left, only `RoomActor::forget`'s in-memory, resident-lifetime set.
- **A blocked room rejects `make_admin`'s own power-levels write too**, unconditionally, via the
  same gate as any other local send. An operator must unblock first.
- **`make_admin` picks the acting sender by highest current power level, ties broken by the
  smaller user ID** -- deterministic, but arbitrary among equals; the spec/Synapse do not mandate a
  specific tie-break.
- **Cluster fencing defaults to installed-nowhere (`None`)** on every construction path in this
  crate; it is purely additive and changes nothing until `hs-cli` calls
  `RoomRegistry::install_fencing`, which this session did not do (out of this track's ownership).
- **The fence-check failure is smuggled out of `hs_kv::transact`'s closure via
  `KvError::Aborted` plus a `Cell`**, rather than widening `transact`'s fixed `Result<T, KvError>`
  closure signature (an `hs-kv` change, out of this crate's ownership) or inventing a second
  transaction. `KvError::Aborted` already exists for exactly "a closure asked for the transaction
  to be aborted, carrying an application-level error."

## Session 6 (2026-09-19): profile-change propagation, and the missing device-list call sites

**Assignment** (explicit cross-track scope for this session only: `crates/hs-room/**`,
`crates/hs-auth/**`, `crates/hs-e2e/**` and both this file and `docs/status/07-auth-and-identity.md`
-- no other crate touched, no Docker, no git). Track 05's "Session 4" (`docs/status/05-sync.md`)
diagnosed both bugs live against a real client and named the exact gaps; this session closes both.

### 1. A profile change now reaches every room the user is joined to

**The bug, exactly as track 05 diagnosed it**: `PUT /profile/{userId}/displayname`/`avatar_url`
(`crates/hs-auth/src/routes/profile.rs`) wrote only `UserRecord::display_name`/`avatar_url` in
`hs-auth`'s own store. `hs-room` (`crates/hs-room/src/routes/membership.rs`) only ever read a
user's profile into a *new* `m.room.member` event at join/invite/knock time -- an already-sent
membership event was never revisited, so a rename was invisible to everyone else forever. Track
05's real-client loadgen scenario carried this as a `KNOWN BUG` line.

**Why this needed three crates' worth of thinking even though only two crates changed code.**
`hs-auth` cannot depend on `hs-room` (`hs-room` already depends on `hs-auth`, for `AuthState` and
to read profiles at join time; the reverse would be a cycle), so whichever crate *writes* the
profile has no way to reach the per-room actors that need to re-stamp their membership event.
Two designs were considered:

- A hook on `AuthState` (the same shape as `RoomRegistry::GlobalTokenResolver`/
  `E2eState::SyncTokenResolver`, and the same shape this session used for the *device-list* half
  below), installed by whichever crate has both an `AuthState` and a `RoomRegistry` in hand. The
  problem: no such call site exists today for rooms the way `E2eState::new(auth, e2e_store)` exists
  for e2e -- `RoomState` is built as a plain struct literal directly in `hs-cli`'s `serve.rs`
  (`RoomState { auth: auth_state.clone(), rooms: rooms.clone(), identity }`), not through a
  constructor function this crate owns. Making that a constructor to hook into would need an
  `hs-cli` edit, which was out of scope this session (another agent holds that crate).
- **Chosen instead**: move the *write* endpoints (`PUT /profile/{userId}/displayname`/`avatar_url`
  only -- the `GET`s stay in `hs-auth`, they need no room context) into `hs-room`'s own router,
  which already embeds a full `AuthState` via `RoomState`. The new handler
  (`crates/hs-room/src/routes/profile.rs`) calls straight back into `hs-auth`'s own
  `put_displayname`/`put_avatar_url` for the actual store write (unchanged, still tested directly
  in `hs-auth`), then fans the room refresh out. `hs-auth`'s router
  (`crates/hs-auth/src/routes/mod.rs`) simply stopped registering a `PUT` for those two paths. Both
  routers are still merged at the same `/_matrix/client/{v3,r0}` prefixes in `hs-cli`'s `serve.rs`,
  **completely unchanged** -- this mirrors the `GET`/`POST /publicRooms` split session 4 already
  recorded between this crate and `hs-user` (different HTTP methods/paths from different crates'
  routers merged at the same prefix coexist without collision; only an identical method *and* path
  registered twice panics at router-build time). Confirmed empirically, not just by precedent: the
  real `hs` binary builds and serves correctly with both changes in place (see "Proof" below).

**The room-actor half** (`crates/hs-room/src/actor.rs`): `RoomActor::refresh_own_profile(user,
display_name, avatar_url, now_ms)` reads the user's current `m.room.member` content, overwrites
only `displayname`/`avatar_url` (removing the key if the new value is `None`), and re-sends it
through the existing `membership_action(Join)` path -- the exact "re-send join while already
joined" mechanism `crate::routes::membership`'s doc comment already described as a manual escape
hatch; this just automates it. Returns `Ok(None)` (sends nothing) if the user is not currently
`join`ed in that room. Goes through `RoomActor::idempotent_state_reuse` for free, so a rename that
does not actually change anything for a given room (a second identical `PUT`) sends no event.

**Finding "every room this user is joined to" without scanning every room on the server.** This
crate never had such an index (session 4's status file explicitly noted the gap: "no
`list_all_room_ids`, unlike the room directory's `list_published_room_ids`"). New durable index:
`Tables::joined_rooms`, a `(user_id, RoomSn) -> b""` keyspace (`crates/hs-room/src/persist.rs`),
maintained inside the *same* KV transaction `RoomActor::persist` already writes every event
through -- one extra `put`/`delete` per `m.room.member` event, keyed on whether its new membership
is `join`. `crate::actor::rooms_joined_by_user`/`RoomRegistry::rooms_joined_by_user` answer the
query with a prefix scan (order-preserving tuple-key encoding means `(user_id,)` is a genuine byte
prefix of `(user_id, RoomSn)` -- verified against `hs-tables`' own encoding scheme, not assumed).

**What a rename costs for a user in many rooms** (the brief's explicit ask): the HTTP response
never waits on the fan-out. `crate::routes::profile::put_displayname`/`put_avatar_url` write the
profile and return as soon as `hs-auth`'s own handler returns; the room-by-room re-stamp runs on a
detached `tokio::spawn`ed background task, which itself bounds its own concurrency to
`MAX_CONCURRENT_ROOM_REFRESHES = 16` rooms at a time via a `tokio::sync::Semaphore` rather than
locking every joined room's actor at once. A user in a thousand rooms produces a thousand cheap,
mostly-idle spawned tasks (each just awaiting a permit), not a thousand-way concurrent burst of KV
transactions -- and each individual room's refresh is one idempotent `send_event`-shaped write, so
running the whole fan-out twice (two rapid profile changes) is safe. Chose a fixed cap over
scaling with room count deliberately: bounding concurrency, not maximizing throughput, was the
point.

**Tests**: `crates/hs-room/src/routes/profile.rs`'s own module
(`put_displayname_refreshes_membership_in_every_joined_room` -- end-to-end through the real
handler and a real second room member, polling up to 5s for the background fan-out to land;
`put_displayname_with_no_joined_rooms_is_a_no_op_fanout`; `put_displayname_for_another_user_is_still_forbidden`,
confirming `hs-auth`'s `403` still surfaces unchanged through the wrapper). `cargo test -p hs-room
--lib`: 50 passed (up from 47); `cargo test -p hs-room --test scenario`: still 11 passed, unchanged.

**Proof against the real thing** (per this session's exact instructions):
```
cargo build -p hs-cli --bin hs
cargo test -p hs-loadgen --test real_client -- --nocapture
```
The scenario's step 14 (previously `KNOWN BUG (not this track's crates, see
docs/status/05-sync.md): alice's profile change never reached bob's /sync as an updated
m.room.member event within the bounded wait`) now reads:
```
bob's /sync saw alice's profile change reflected in her m.room.member event
```
**The `KNOWN BUG` line is gone.** `cargo test -p hs-loadgen --test real_client_encrypted --
--nocapture` still passes all 16 steps unchanged (no regression from either fix in this session).

### 2. Device-list changes are now recorded from every call site that changes a device

**The bug, exactly as diagnosed**: `record_device_list_change` (`hs_e2e::store::DeviceKeyStore`)
was called from exactly one place, cross-signing bootstrap
(`crates/hs-e2e/src/routes/cross_signing.rs`). Neither an ordinary `/keys/upload` (identity key
upload -- the common case) nor `hs-auth`'s device rename/delete ever bumped the stream, so another
user's client never learned a device appeared, was renamed, or vanished.

**The same cross-crate shape as above, solved the same way `hs-e2e` already solves it for its own
sync-token problem.** `hs-auth` cannot depend on `hs-e2e` (`hs-e2e` already depends on `hs-auth`).
New `DeviceListChangeNotifier` trait and hook on `AuthState`
(`crates/hs-auth/src/state.rs`) -- installed by `hs_e2e::state::E2eState::new` as a side effect of
construction (`crates/hs-e2e/src/state.rs`'s `AuthDeviceListNotifier` adapter, wrapping this
crate's real `Arc<dyn E2eStore>`). **No `hs-cli` change needed**: `E2eState::new(auth.clone(),
e2e_store)` is already the one call site in `serve.rs` with both an `AuthState` and an `Arc<dyn
E2eStore>` in hand, already called unchanged before this session. Full writeup of this half (the
`hs-auth` side of the hook, its tests) is in `docs/status/07-auth-and-identity.md`'s new "Session
6" section, since it is squarely that crate's surface even though this session made the edit.

**The three missing call sites, all now wired**:
- `crates/hs-auth/src/routes/devices.rs`: `put_device` (both the ordinary rename branch and the
  MSC4190 appservice-create branch), `delete_device`, and `post_delete_devices` (one notification
  for the whole batch, not one per device -- see that file's own comment) all call the new
  `AuthState::notify_device_list_changed` after their store mutation succeeds.
- `crates/hs-e2e/src/routes/keys_upload.rs`: `post_keys_upload` now calls
  `store.record_device_list_change` after `upload_device_keys` succeeds -- scoped to a request that
  actually included `device_keys` (identity keys), not a one-time/fallback-key-only top-up, which
  changes available key material but not what the device *is*. Matches Synapse's own
  `_upload_keys`, which only calls `notify_device_update` on the device-keys branch.

**Tests**: `crates/hs-auth/src/routes/devices.rs` gained a `RecordingNotifier` test double and six
new tests covering rename, MSC4190 create, delete, "UIA not yet satisfied does not notify", bulk
delete (exactly one notification), and "empty bulk-delete list does not notify". `cargo test -p
hs-auth`: 181 passed (up from 175). `crates/hs-e2e/src/state.rs` gained
`constructing_e2e_state_wires_auth_state_notifications_into_this_crate_store` (the real,
non-mocked wiring: an `AuthState::notify_device_list_changed` call with no reference to `hs-e2e`
anywhere at the call site reaches this crate's real `TablesE2eStore`) and
`notify_is_a_no_op_before_any_e2e_state_installs_a_notifier`. `crates/hs-e2e/src/routes/keys_upload.rs`
gained `uploading_device_keys_bumps_the_device_list_stream` and
`uploading_only_one_time_keys_does_not_bump_the_device_list_stream`. `cargo test -p hs-e2e`: 36
passed across all four test binaries (up from 27+ baseline; no regressions in
`complement_regressions.rs`, `otk_concurrency.rs` or `scenario.rs`).

**Proof against the real thing**: `cargo test -p hs-loadgen --test real_client_encrypted --
--nocapture` (which does a real, unscripted `/keys/upload` for both clients, not a synthetic
fixture) still passes all 16 steps, including "bob's `/sync` reported alice's device-list change in
`device_lists.changed`" -- that assertion was already passing before this session (bootstrapping
cross-signing, the one pre-existing call site, is also part of that scenario), so it does not by
itself prove the *new* call sites fire; the new unit tests above are what prove that directly,
since `real_client`/`real_client_encrypted` do not currently script a bare key upload or a device
rename/delete as a scenario step. Recorded as a gap for whichever session next extends the loadgen
scenarios, not fixed here (`hs-loadgen` was explicitly off limits to edit this session, per the
assignment -- only running it was allowed).

### How to verify (session 6)

```
cargo fmt -p hs-room -p hs-auth -p hs-e2e
cargo clippy -p hs-room --all-targets --no-deps -- -D warnings
cargo clippy -p hs-auth --all-targets --no-deps -- -D warnings
cargo clippy -p hs-e2e --all-targets --no-deps -- -D warnings
cargo test -p hs-room --lib      # 50 passed
cargo test -p hs-room --test scenario   # 11 passed
cargo test -p hs-auth             # 181 passed
cargo test -p hs-e2e              # 36 passed across 4 binaries
cargo build -p hs-cli --bin hs
cargo test -p hs-loadgen --test real_client -- --nocapture             # KNOWN BUG line gone
cargo test -p hs-loadgen --test real_client_encrypted -- --nocapture   # unchanged, still green
```

Used `--no-deps` on clippy for the same reason session 5's writeup recorded: other tracks' crates
in the same dependency graph can transiently fail `-D warnings` on their own unrelated lints while
being actively edited in parallel; `--no-deps` scopes enforcement to the crate actually requested.
Not observed to be a real problem this session (both plain and `--no-deps` clippy invocations were
clean when tried), noted here defensively since sessions 4 and 5 both hit it.

### Interfaces provided (new this session)

- **`hs_room::actor::rooms_joined_by_user`/`RoomRegistry::rooms_joined_by_user`**: every room a
  user currently holds `join` membership in, via a prefix scan over the new
  `Tables::joined_rooms` index -- no other crate needs this today, but it is the kind of query
  `hs-user`'s own "does this user have any rooms at all" checks might eventually want instead of
  re-deriving it.
- **`hs_room::actor::RoomActor::refresh_own_profile`/`RoomActorHandle::refresh_own_profile`**:
  re-stamps a user's own `m.room.member` event with a fresh profile, a no-op if they are not
  currently joined.
- **`hs_auth::state::DeviceListChangeNotifier`/`AuthState::install_device_list_notifier`/
  `notify_device_list_changed`**: the hook other tracks' crates should use if they ever need to
  learn about a device mutation from `hs-auth`'s own routes without `hs-auth` depending on them --
  same pattern as `RoomRegistry::GlobalTokenResolver`/`E2eState::SyncTokenResolver`.

### Interfaces needed

Nothing new from this session. Everything already listed in session 5's "Interfaces needed"
(pagination-token format mismatch between `hs-user` and this crate, `forgotten`/directory-publish
query surfaces for `hs-user`) is unchanged.

### Decisions made this session

- **The profile write endpoints (`PUT` only) moved from `hs-auth`'s router to `hs-room`'s**, while
  the underlying `UserRecord` write logic and its own tests stay in `hs-auth`, called into
  directly. Chosen over an `AuthState` hook (the pattern used for device-list changes, item 2)
  because no existing call site in `hs-cli` builds both an `AuthState` and a `RoomRegistry`
  together the way `E2eState::new` builds an `AuthState` and an `E2eStore` together -- inventing
  one would have needed an `hs-cli` edit, out of scope this session.
- **`Tables::joined_rooms` is maintained inside `RoomActor::persist`'s existing transaction**, not
  as a separate write after the fact, so it can never observe a torn state relative to the
  `m.room.member` event it derives from.
- **The background profile-refresh fan-out is capped at 16 concurrent rooms**, a fixed constant
  rather than scaling with the user's room count -- see "What a rename costs" above.
- **`post_delete_devices` sends one device-list notification per batch, not per device.**
- **`/keys/upload` only bumps the device-list stream when the request includes `device_keys`**,
  not for a one-time/fallback-key-only top-up -- matches Synapse's own `_upload_keys` behavior and
  the actual semantics (a device's *identity* changed, not just its available key material).
- **No `hs-cli` edit was made or needed for either fix.** Both hooks are installed by an existing,
  unchanged call site inside a crate this session already owned (`hs-room`'s own router
  registration for item 1; `hs-e2e`'s `E2eState::new` for item 2).

## Session 5 (2026-09-19): signing fix, `/threads`, `/upgrade`, idempotency, `unsigned.transaction_id`

**Starting point.** Assigned, in value order: (1) whole endpoints 404ing --
`/relations`/`/threads`/`/search`/room `/upgrade`; (2) diagnose a `/forget` regression in
`/sync`'s `left` section (track 05's file, not this crate's to fix); (3) non-idempotent state/join
(sending the same state event or join twice creates a second event); (4) missing
`unsigned.transaction_id`; (5) alias/canonical-alias validation and `/state`/`/joined_members`
shape gaps. Mid-session, the integration lead interrupted with a higher-priority, independently
verified bug in this crate (`docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md`) and
asked for it first; it is fixed and tested (see item 0 below), then the rest of the list was
picked back up. Item 5 (alias/canonical-alias validation, `/state`/`/joined_members` shape gaps)
was not reached this session -- out of time, not attempted.

### 0. The signing bug: events were signed unredacted, not redacted (urgent, fixed first)

**The bug**, per `docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md` (written by track
06 after it independently found and fixed the symmetric bug on the *verification* side,
`hs_federation::inbound::verify_pdu`): the spec's signing algorithm is hash the full event,
**redact** it, sign the **redacted** object, then copy the resulting signature back onto the
original unredacted event. `crate::pipeline::build_and_authorize`'s hash-and-sign step computed
the content hash correctly but then called `signing::sign_object` directly on the **unredacted**
object -- no redaction in between. Since redaction is deterministic from an event's `type` alone
and strips *all* of `content` for any type without a special case (`m.room.message` among them,
via `hs_model::redaction::redact_content`), every ordinary message this server has ever
originated carried a signature that a spec-compliant remote homeserver -- which always redacts
before verifying, per the matching "Validating hashes and signatures" text -- would reject.
Outbound federation had likely never been verifiable by a compliant peer, independent of the
TLS/CA gap track 14 found the same session.

**The fix** (`crates/hs-room/src/pipeline.rs`, the hash-and-sign block): redact the canonical
object (`hs_model::redaction::redact(&canonical, &rules.redaction)`), sign *that* copy, then move
the resulting `signatures` entry back onto the real, unredacted `canonical` this function returns
and persists. Three lines added, matching the exact shape the RFC specified and the pattern
already used in `hs_federation`'s own test helpers.

**The test that would have caught it**:
`crate::actor::tests::a_sent_message_is_signed_over_its_redacted_form_not_the_full_event`. Sends a
real `m.room.message` through the actual `RoomActor::send_event` path (not a hand-built test
fixture), redacts the persisted event's own JSON independently, and asserts three things: (1)
`hs_model::signing::verify_object` succeeds against the *redacted* form (the spec-compliant check
-- this is what would have failed before the fix); (2) the full, unredacted event's `content` is
still intact for an ordinary client read (the fix must not touch what gets stored/served, only
what gets hashed for the signature); (3) as a sanity check that the test is actually exercising
the bug, the full unredacted object's `content` genuinely differs from the redacted form's, and
verifying the signature against the *full* object fails (proving the signature really is scoped
to the redacted bytes, not "verifies against anything"). All three of `builds_and_authorizes_a_create_event`,
this new test, and the rest of `cargo test -p hs-room` (48 tests before this session, now 58) stay
green with the fix in place.

Left for another session/track: `crates/hs-cli/tests/federation_writes.rs`'s three test-fixture
call sites that sign a synthetic PDU the same wrong way (mechanical, same three-line fix, and
explicitly not this crate's file to touch -- the RFC names it, and the integration lead said
another agent holds `hs-cli` and would fix those fixtures directly).

### 1. `/threads` (`GET /rooms/{roomId}/threads`, client-server API "Threading", v1.4)

**New**: `crate::actor::RoomActor::thread_roots(requester, participated_only) -> Vec<&Event>` --
every event that is the target of at least one `m.thread` relation, most-recently-active-thread
first (ordered by the latest `m.thread` child's `origin_server_ts`, descending;
`room_threads_test.go`'s `TestThreadsEndpoint` checks this ordering directly, including that a new
reply to an older thread moves it back to the front). `crate::routes::threads::get_threads` wraps
it with history-visibility filtering (`event_visible_to`), a plain-decimal-offset pagination token
(thread order is "most active first", not a timeline position, so the existing
`crate::timeline::PaginationToken` does not apply), and the same bundled-aggregation +
`unsigned.transaction_id` rendering every other read path uses. Registered in
`crate::routes::router` as `GET /rooms/{roomId}/threads`.

**A real bug found and fixed along the way**: the threading module's `current_user_participated`
rule is "true when the user is *either* (1) the sender of the thread root, *or* (2) the sender of
an `m.thread` child" (`refs/matrix-spec/content/client-server-api/modules/threading.md`). The
existing `crate::relations::bundle` (used by `/relations`, `/event`, `/messages`, `/context`
already) only implemented rule 2 -- a user who started a thread but never replied to it was
reported as not having participated in their own thread. Fixed by threading the root event's
sender into `bundle()` as a new `Option<&UserId>` parameter (`crate::actor::RoomActor::relation_bundle`
now looks it up and passes it through). This affects every endpoint that renders
`unsigned.m.relations.m.thread.current_user_participated`, not just the new `/threads` endpoint.

**Tests**: `crate::relations::tests::bundle_counts_the_thread_root_sender_as_participating_even_without_a_reply`
(the participation-rule fix, isolated); `crate::actor::tests::thread_roots_are_ordered_by_latest_reply_and_reorder_on_a_new_reply`
and `crate::actor::tests::thread_roots_participated_filter_matches_the_threading_module_rules`
(the new endpoint's core logic, at the actor level).

### 2. `/upgrade` (`POST /rooms/{roomId}/upgrade`)

Implemented per the spec's documented server behaviour
(`refs/matrix-spec/content/client-server-api/modules/room_upgrades.md`), locally only (no
federation -- out of this crate's scope): validates the target room version
(`hs_model::room_version::rules_for`) before any side effect; mints the replacement room's ID
up front (`ruma::RoomId::new_v1`) so both the old room's tombstone (`content.replacement_room`)
and the new room's create event (`content.predecessor.event_id`) can name each other without a
two-phase build-then-persist dance; sends the tombstone to the old room (which is also the
permission check -- an unauthorized sender gets `RoomError::Forbidden` from the ordinary
event-authorization path and nothing else in the handler runs); creates the replacement room via
`RoomActor::create_room` reusing the recommended transferable state
(`crate::actor::RoomActor::transferable_state`: `m.room.server_acl`, `encryption`, `name`,
`avatar`, `topic`, `guest_access`, `history_visibility`, `join_rules`, `power_levels`, whichever
of these the old room actually has set) and the create event's `type` field
(`crate::actor::RoomActor::creation_type`); moves local aliases and, best-effort, the canonical
alias content; and, best-effort (the spec's own "if possible" hedge), locks the old room's power
levels down (`events_default`/`invite` raised to `max(50, users_default + 1)`).

**New in `crate::actor::CreateRoomRequest`**: a `room_id: Option<OwnedRoomId>` field -- `None`
(every existing caller, via `..Default::default()`) means "generate one" exactly as before;
`/upgrade` is the only caller that sets it, since it must know the replacement room's ID before
creating it. Additive, no existing call site needed more than the `..Default::default()` it
already had.

**Not implemented** (documented scope narrowing, not oversight): push-rule migration for local
users (Synapse/Dendrite do this; it needs `hs-push` and `hs-user` cooperation and is the subject
of `room_upgrade_test.go`'s only real Complement test, `TestPushRuleRoomUpgrade` -- which also
needs two-homeserver federation, out of reach for a single-crate fix regardless); MSC4291
`additional_creators` handling (spec v1.16, deliberately out of scope the same way room version
12's own hash-based room ID support already is, per session 3's decision).

**Tests**: `crate::routes::upgrade::tests::upgrade_creates_a_replacement_room_with_predecessor_and_tombstone`
(end-to-end through the real route handlers -- `RoomRequester` constructed directly rather than
through HTTP/auth middleware, since nothing about this handler's logic depends on how the
requester was authenticated; checks the new room's version, its `predecessor`, that `m.room.topic`
was carried over, and that the old room's tombstone names the new room) and
`crate::routes::upgrade::tests::upgrade_to_an_unsupported_version_is_rejected_before_any_side_effect`
(no tombstone, no replacement room, when the version check fails first).

### 3. `/relations` and `/threads` both 404 in the real deployed server -- and it is not this crate's bug

**`/relations` was already fully implemented** (`crate::routes::relations`, `crate::relations`
module, session before this one or earlier -- this session found it working and complete, needing
only the participation-rule fix in item 1 above) when this session started, yet
`docs/status/14-test-and-conformance.md`'s Complement run and this session's own assignment both
named it as 404ing. **Root cause, confirmed by reading `crates/hs-cli/src/serve.rs`**: the client-server
spec places `/relations` and `/threads` at basePath `/_matrix/client/v1`
(`refs/matrix-spec/data/api/client-server/relations.yaml` and `threads_list.yaml`'s `servers:`
block; Complement's own test code hits them there --
`refs/complement/tests/csapi/room_relations_test.go`/`room_threads_test.go` call
`["_matrix", "client", "v1", "rooms", roomID, "relations", ...]` verbatim, never `v3`), but
`hs-cli/src/serve.rs` mounts `hs_room::routes::router()` (this crate's whole router fragment,
including `/relations` and the new `/threads`) only at `/_matrix/client/v3` and
`/_matrix/client/r0` (lines ~277-282). It already mounts *other* crates' routers at v1
(`/_matrix/client/v1/media`, `/_matrix/client/v1` for appservice ping) -- the infrastructure
exists, this crate's router is just never merged into it there. **Every request Complement (or
any client following the spec literally) makes to `/_matrix/client/v1/rooms/{roomId}/relations/...`
or `/threads` 404s today, regardless of how correct this crate's own handlers are**, because axum
never routes the request to them at all.

**This is `crates/hs-cli/src/serve.rs`, explicitly out of this track's ownership this session**
(another agent holds it). The fix is one line, added next to the existing v1 merges:
```rust
.merge_router("/_matrix/client/v1", room_router.clone(), room_routes.clone())
```
(reusing the same `room_router`/`room_routes` values `serve.rs` already builds and merges at v3/r0
a few lines above -- see that file's existing `.merge_router("/_matrix/client/v3", room_router.clone(), room_routes.clone())`
call). **Flagging for the integration lead / whoever next holds `hs-cli`**: this single line is
very likely the actual fix for the `/relations` and `/threads` items on this track's list, more
than anything achievable by editing `crates/hs-room` alone.

### 4. `/search` -- skipped

Not attempted. `POST /search` (not room-scoped) needs to enumerate every room a requesting user
has ever been a member of and full-text-match `content.body`/`content.name`/`content.topic`
across all of them. This crate's per-room actor model has no such cross-room index today: a
`RoomRegistry` only knows the rooms it has loaded or created in this process, not "every room
this server hosts" (there is no `list_all_room_ids`, unlike the room directory's
`list_published_room_ids`, which is a much narrower, already-solved case), and there is no
per-user "rooms I have joined" index anywhere in this crate either (that lives on the `hs-user`
side, or would need `hs-01-storage`'s tantivy integration per `PLAN.md`). Implementing a
correct-but-slow version (load every room this server has ever created, scan every event) would
be a lot of new surface for a single session to get right and verify, for a feature `PLAN.md`
already earmarks for a real search index elsewhere. Left for a session with more room to build
(and verify) that index properly, or for whichever track ends up owning full-text search
infrastructure.

### 5. Non-idempotent state and join, fixed

**The bug** (`rooms_state_test.go`'s "Setting state twice is idempotent" and "Joining room twice
is idempotent", ported from sytest): `PUT /rooms/{roomId}/state/{eventType}(/{stateKey})` has no
`{txnId}` in its path at all (unlike `/send`/`/redact`), so the existing transaction-ID dedup
machinery (`RoomActor::send_event_txn`/`redact_txn`) never applied to it, and a retried state PUT
-- or a retried `POST /join` (`membership_action` also goes through `send_event`) -- created a
second event every time, even when the content was byte-for-byte identical to the room's current
state for that `(event_type, state_key)`.

**The fix**: `crate::actor::RoomActor::send_event` (the shared path both direct state sends and
`membership_action`'s join call go through -- not `send_event_citing`, the lower-level primitive
this crate's own fork tests deliberately need to *not* dedupe) now checks, when `state_key` is
`Some`, whether the room's current event for that `(event_type, state_key)` already carries
exactly `content` (structural `serde_json::Value` equality -- insensitive to key order, since one
side round-trips through canonical JSON and the other is the raw client body). If so, it returns
the existing event instead of building a new one. This one check covers both halves of the bug
for free: an unchanged repeated join produces byte-identical `membership_action`-constructed
content (assuming no profile change happened in between -- see session 4's profile-propagation
design, which intentionally still allows a *changed* re-join to mint a new event), so the same
equality check that fixes ordinary state idempotency fixes join idempotency too, with no
join-specific special case needed.

**Tests**: `crate::actor::tests::setting_the_same_state_twice_is_idempotent` (identical content ->
same event ID; genuinely different content -> a real new event, so the fix isn't over-broad) and
`crate::actor::tests::joining_a_room_twice_is_idempotent`.

### 6. `unsigned.transaction_id`, added

**The gap**: a client that sends an event through a `{txnId}`-suffixed endpoint
(`PUT .../send/{txnId}`, `PUT .../redact/{txnId}`) needs that transaction ID echoed back on its
own later view of the event (`unsigned.transaction_id`) to match its optimistic local echo against
the real event -- client-server API "Local echo" / `txnid_test.go`. This crate recorded the
`(sender, device, txnId) -> event_id` mapping for dedup (`RoomActor::txn_dedup`, session 4) but
never surfaced it back out through any read path, and every rendered event's `unsigned` was always
just `{}` (or the relations bundle) regardless of who was asking.

**The fix**: a new reverse index, `RoomActor::event_txn: HashMap<EventId, (sender, device,
txn_id)>`, populated alongside `txn_dedup` in a new shared `RoomActor::record_txn` helper (used by
both `send_event_txn` and `redact_txn`, replacing their previous direct `txn_dedup.insert` calls).
`RoomActor::transaction_id_for(event_id, viewer, viewer_device)` looks it up and returns `Some`
only when the viewer's `(user_id, device_id)` matches the `(sender, device)` that actually sent it
-- device-scoped, not access-token-scoped, per `txnid_test.go`'s `TestTxnScopeOnLocalEcho` (two
sessions sharing one device ID, e.g. a relogin without a new device or a refreshed access token,
must see the same transaction ID; a different device of the same user, or any other user, must
never see it, even for the exact same event). `crate::routes::render::attach_transaction_id` is
the new rendering helper, wired into every event-rendering call site that has a requester in scope:
`get_event`, `get_context` (the target plus `events_before`/`events_after`), `get_messages`, and
the new `/threads` endpoint.

**Not surfaced**: `/sync`'s own timeline rendering (`hs-user`, track 05) needs the identical
lookup for its own timeline events. `RoomActor::transaction_id_for` is `pub` specifically so that
crate can call it through `RoomActorHandle::query` exactly the way this crate's own routes do --
no new interface needed, just documenting that it exists (see "Interfaces provided" below).

**Tests**: `crate::actor::tests::transaction_id_for_is_scoped_to_sender_and_device` (all four
cases: sending device sees it, a different device of the same user does not, a different user
does not even with the same device ID, and an ordinary state event -- never sent through a
`{txnId}` endpoint -- never carries one) and the HTTP-level
`crate::tests::transaction_id_is_echoed_back_to_the_sender_only` in `tests/scenario.rs` (real
`PUT .../send/{txnId}` then `GET .../event/{eventId}` as both the sender and a different user).

### 7. The `/forget` regression in `/sync`'s `left` section -- diagnosed, not fixed (track 05's file)

**Per this session's instructions, diagnosed rather than fixed** (the bug lives in `hs-user`,
track 05's crate, which was in flight this session). **Root cause, found by reading this crate's
own `RoomActor::forget`**: forgetting a room (`POST /rooms/{roomId}/forget` ->
`RoomActor::forget` -> `self.forgotten.insert(user)`) is **entirely invisible outside this
actor**. It does not publish a `RoomUpdate` on the room's broadcast stream (nothing about
forgetting changes the room's state or timeline -- there is no event to publish), and there is no
public accessor at all for "has `user` forgotten this room" (`forgotten: HashSet<OwnedUserId>` is
a private field; only this actor's own `can_read_room`/read-path checks consult it). If `hs-user`'s
`/sync` determines whether a left room should keep appearing under `left` by watching this
crate's `RoomUpdate` stream and/or membership history alone (the natural design, and the only
public surface this crate currently offers), it has **no way to ever learn that a forget
happened** -- so a room a user left *and then forgot* has no mechanism to stop appearing in
`left`, because nothing ever told `hs-user` the forget occurred. This matches the reported
symptom exactly ("a forgotten room is not leaving `/sync`'s `left` section correctly") and does
not require any assumption about `hs-user`'s internals to explain -- the gap is fully on this
crate's side of the interface, in what it fails to expose, even though the actual code that needs
to change to consume it is track 05's.

**Recommended fix, for whichever session picks this up** (not implemented this session --
out of time after the higher-priority items above, and the instructions for this session asked
for a diagnosis, not a fix, on this specific item): add a public
`RoomActor::has_forgotten(&self, user: &UserId) -> bool` accessor (a one-line wrapper over the
existing private `forgotten.contains`) so `hs-user` can check it directly through
`RoomActorHandle::query`, the same pattern every other cross-crate read already uses in this
codebase (`transaction_id_for`, `event_visible_to`, ...). Whether `hs-user` should poll this at
sync time or whether `RoomActor::forget` should *also* start publishing a synthetic `RoomUpdate`
(so a long-poll `/sync` in flight wakes up immediately instead of only reflecting the forget on
its *next* poll) is a design call for track 05, not something to guess at from this side of the
interface.

## How to verify (session 5)

```
cargo fmt -p hs-room -- --check
cargo clippy -p hs-room --all-targets --no-deps -- -D warnings   # --no-deps: hs-admin (track 15,
                                                                  # in flight this session) fails
                                                                  # -D warnings on its own unused
                                                                  # imports; not this crate's bug,
                                                                  # see "Blockers"
cargo test -p hs-room
```

58 tests today (up from 48 at session start): 47 unit/property tests (`cargo test -p hs-room
--lib`, up from 38) plus 11 `hs-testkit` scenario tests (`cargo test -p hs-room --test scenario`,
up from 10) -- all green as of this session's last run.

## Blockers

**Transient, not this crate's fault, noted for whoever hits it next**: `cargo clippy -p hs-room
--all-targets -- -D warnings` (without `--no-deps`) fails today because `hs-admin` (track 15,
actively being edited this session) has an unused-import warning that `-D warnings` promotes to a
hard error for the whole invocation, even though only `hs-room` was requested with `-p`. Use
`--no-deps` to scope enforcement to this crate alone (verified clean); the plain form should start
working again once track 15's edit settles. Similarly, mid-session, `cargo test -p hs-room --test
scenario` briefly failed all 11 tests with `401 Unauthorized`/UIA-required on registration (an
in-flight `hs-auth`/track 07 change to `POST /register`'s UIA requirements, reproduced twice,
confirmed unrelated to anything in this crate via `cargo test -p hs-loadgen --test real_client`
failing identically against a real `matrix-rust-sdk` client) -- this has since resolved itself
(the same 11 tests pass again as of this session's final run) without this crate changing at all.
Neither blocker needed or received a workaround in this crate.

## Session 4 (2026-09-19): fixing this track's largest Complement failure cluster

**Starting point.** `docs/status/14-test-and-conformance.md`'s first-ever Complement run (csapi
package) scored 125/293 leaf assertions passing, and named this track as owner of the largest
failure cluster: history-visibility not enforced on reads (a real security bug), the room
directory 404ing, `POST /createRoom` accepting invalid parameters, and `POST /forget` not
validating membership. This session closed all four, plus the optional fifth item (extensible
`m.topic`, MSC3765), and found one more read path with the same history-visibility bug the brief
didn't explicitly name (`GET /state`, `GET /state/{eventType}(/{stateKey})`, `GET /members`).
Scope: **`crates/hs-room/**` and this status file only** -- every other crate was off limits
(other tracks in flight); one genuine cross-track collision was hit and resolved without editing
anything outside this crate (see "The `/publicRooms` collision with track 05" below).

### 1. History visibility enforced on every read path (the security fix)

**The bug.** `GET /rooms/{roomId}/messages`, `GET /rooms/{roomId}/event/{eventId}`,
`GET /rooms/{roomId}/context/{eventId}`, `GET /rooms/{roomId}/state(/...)"` and
`GET /rooms/{roomId}/members` all read the room's **current** resolved state unconditionally,
regardless of the requester's own membership history. A user who had left a non-world-readable
room kept full read access to everything, including events and state changes that happened
*after* they left -- the read-side half of `m.room.history_visibility` (the module at
`refs/matrix-spec/content/client-server-api/modules/history_visibility.md`, CC-BY-4.0) was simply
never implemented; only the write side (auth-checking new events, in `hs-state`, track 02's crate)
existed.

**The fix, per event (`GET .../event`, `.../context`, `.../messages`).**
`crates/hs-room/src/history_visibility.rs` is a new, pure module porting the spec's "Server
behaviour" section verbatim: `HistoryVisibility::{WorldReadable, Shared, Invited, Joined}` and
`base_rule_allows(visibility, membership, joined_later) -> bool`, implementing rules 1-5 exactly
(world_readable always allows; a `join` membership always allows regardless of visibility; shared
allows if the user joined at any point *after* the event was sent, even if they've since left
again; invited allows an `invite` membership; otherwise deny).
`crates/hs-room/src/actor.rs::RoomActor::event_visible_to(event, requester)` supplies the two state
snapshots this needs -- "state resolved from `event`'s own `prev_events`" and "state at `event`"
(`hs_state::api::StateStore::state_at`, already available; state history was never the underlying
gap, just never surfaced through a read path) -- plus a forward timeline scan for rule 3's "joined
later", and applies the spec's two special cases: for an `m.room.history_visibility` event itself,
allow if the visibility *before or after* the change would allow it; for the requester's *own*
`m.room.member` event, allow if their membership *before or after* the transition would allow it
(so a user can always see their own leave event, the *m.room.name*-for-a-departed-room test below
depends on this).

`get_event`/`get_context` deny by returning `RoomError::EventNotFound` (404), not `Forbidden`
(403): the spec's algorithm makes no distinction between "this event does not exist" and "you may
not see it", so neither does this response (matches
`apidoc_room_history_visibility_test.go`'s expected 404s exactly, and avoids leaking whether an
event exists to someone not allowed to see it). `get_context` filters `events_before`/
`events_after` individually (an invisible one is simply omitted, not an error) but 404s outright if
the *target* itself is invisible.

**The fix, for `.../messages` specifically -- a second, coarser gate on top.**
`RoomActor::can_read_room(requester)` answers "may this requester call `.../messages` on this room
at all", *before* any per-event filtering: `world_readable` always allows; otherwise, a requester
with **no** `m.room.member` event in the room's current state at all (never joined, invited,
knocked, banned -- truly never touched the room) is refused outright with `403 M_FORBIDDEN`
(`room_messages_test.go`'s "you aren't a member of the room"; also applied to a room that does not
exist at all, so `TestFetchMessagesFromNonExistentRoom` gets the same 403 instead of leaking a 404
that would prove the room's absence). A **forgotten** room (see item 4) is refused the same way,
even though `shared` visibility's per-event rule would otherwise let a past member read what they
saw while joined -- see item 4 for why. Once past this gate, `get_messages` filters the fetched
page one event at a time through `event_visible_to`, same as `.../event`.

**The fix nobody asked for but the same bug required: `GET /state`, `GET
/state/{eventType}(/{stateKey})`, `GET /members`.** Complement's `room_leave_test.go`
(`TestLeftRoomFixture`) demonstrated the identical bug on these three endpoints: a departed member
asking for the room's state or member list got the room's *live* state (including changes made,
and members who joined, after they left), not the state as of when they left. Fixed with
`RoomActor::reader_view(requester)`: if the requester is currently joined, or the room is
`world_readable`, returns the live `RoomStateView`; otherwise (requester has left/been banned)
returns a view pinned to the state as of *immediately after their own last membership-changing
event* (`state_view_at_sn` on that event's `EventSn` -- the same `state_at` primitive
`event_visible_to` uses), which is exactly "what they were allowed to see before they left, and
nothing after" without needing per-event filtering for this bulk case. `Ok(None)` denies outright
(no membership record at all, non-world-readable room). `RoomActor::full_state_for_reader`,
`state_event_for_reader` and `members_for_reader` wrap this for `crate::routes::query`'s
`get_state`, `get_state_with_key` and `get_members` respectively. **Not touched**:
`GET /joined_members` (spec only ever means "currently joined", no historical version makes
sense), and `GET /context`'s own `state` field (still reads live current state, a separate,
narrower version of the same bug this session ran out of time for -- noted below).

**Proof.** New scenario tests in `crates/hs-room/tests/scenario.rs`:
`history_visibility_joined_hides_events_sent_after_a_member_leaves` (a `joined`-visibility room:
bob sees a message sent while he was in, not one sent after he left, on both `/messages` and
direct `/event`), `a_stranger_to_a_shared_visibility_room_is_denied_reads` (never-a-member gets 404
on `/event`, 403 on `/messages`), `departed_member_sees_state_and_members_as_of_when_they_left`
(bob's `/state`, `/state/m.room.name`, `/members` all show the pre-departure snapshot; alice, still
joined, sees the live one). Plus `history_visibility.rs`'s own unit tests for the pure rule table.
Against real Complement (`apidoc_room_history_visibility_test.go`, `room_leave_test.go`): all seven
`TestFetch*` history-visibility tests pass, and `TestLeftRoomFixture`'s state/members-for-a-departed-room
subtests pass (its two `.../messages` subtests still fail, but for an unrelated, pre-existing
reason -- see "What is left" below).

### 2. The room directory: a real publish flag, and a collision with track 05

**What's real.** `PUT`/`GET /_matrix/client/v3/directory/list/room/{roomId}`
(`crates/hs-room/src/routes/directory.rs`) are new, backed by a real, durable publish flag:
`crate::persist::Tables::public_rooms` (a new `(RoomSn,) -> b""` keyspace; presence means
published), `RoomActor`-free functions `set_directory_visibility`/`is_directory_public`/
`list_published_room_ids` (`crate::actor`, following the same free-function-over-`(backend,
tables)` pattern as `resolve_alias`/`find_event_globally`, since directory membership is a
server-local administrative fact, not room state visible to other members or servers), and thin
`RoomRegistry` wrappers. `POST /createRoom` gained a `visibility` field (`"public"`/`"private"`,
default `"private"`, validated against exactly those two strings): `visibility: "public"`
publishes the room immediately after creation, independent of `preset` (these are genuinely
orthogonal per the spec -- a `private_chat`-preset room can still be published, keeping its
`join_rule: "invite"`).

**`GET`/`POST /publicRooms` (`crate::routes::directory::{get,post}_public_rooms`) render the
listing from this flag**: for each published room ID, load its actor and build a
`PublicRoomsChunk` (`room_id`, `num_joined_members`, `world_readable`, `guest_can_join`,
`join_rule`, and `name`/`topic`/`canonical_alias`/`avatar_url` *omitted* when unset rather than
sent as empty strings -- `public_rooms_test.go`'s "Name/topic keys are correct" checks exactly
this). `POST`'s `filter.generic_search_term` does a case-insensitive substring match against
`name`/`topic`/`canonical_alias`.

**The `/publicRooms` collision with track 05, and how it resolved.** Partway through this session,
building the real `hs` binary to run the loadgen regression net panicked at startup:
`hs-http`'s `Builder` (shared across every track's router fragment, merged in
`crates/hs-cli/src/serve.rs`) rejects two handlers registered for the same method+path, and
`hs-user` (track 05) had *also* independently implemented `GET`/`POST /publicRooms`
(`crates/hs-user/src/routes/rooms.rs`, backed by its own `UserStore::list_public_rooms`, populated
from `m.room.join_rules == "public"` via `hub.rs`'s `public_directory_entry` -- a materially
different and less spec-correct heuristic: it conflates "publicly joinable" with "listed in the
directory", so a `private_chat`-preset room published via `visibility: "public"` would never show
up for it). Track 05 hit this same collision independently (their own `crates/hs-user/src/routes/mod.rs`
carries a doc comment recording it) and resolved it *from their side* by leaving their
implementation unmounted, deferring to this crate's. This session's router
(`crates/hs-room/src/routes/mod.rs`) is therefore the one that mounts `GET`/`POST /publicRooms` in
the real server. **No file outside `crates/hs-room/**` was edited to resolve this** -- it resolved
itself once both tracks' sessions landed in the same working tree, which is worth recording as a
case study in why the workspace's "leave a clear seam, document don't coordinate live" convention
works. `hs-user`'s implementation is left in place (unmounted) by that crate's own choice, in case
it has something worth merging in later.

**Proof.** `crates/hs-room/tests/scenario.rs::room_directory_publish_and_unpublish_round_trip`:
publish via `PUT`, confirm via `GET .../directory/list/room` and the registry directly, confirm it
appears in `GET /publicRooms` with the right fields, confirm the `POST` search filter finds it,
unpublish, confirm it disappears, confirm `POST /createRoom`'s own `visibility: "public"` publishes
without a separate call, confirm a malformed `visibility` value is `M_BAD_JSON` and publishing a
nonexistent room 404s. Against real Complement (`public_rooms_test.go::TestPublicRooms`): both
subtests ("Can search public room list", "Name/topic keys are correct", including all seven
alias/name/topic/unicode variants) pass.

### 3. `POST /createRoom` validates its request shape

`crates/hs-room/src/routes/create_room.rs`: `room_version` must be a JSON *string* if present at
all -- a syntactically-valid-but-wrong-typed value (a bare number, the sytest/Complement case) is
now `400 M_BAD_JSON` instead of being silently ignored via `.and_then(Value::as_str)` returning
`None` and falling back to the default version (the room got created anyway, `200 OK`, where the
spec test wants `400`). A well-typed-but-unrecognized version string (`"ahfgwjyerhgiuveisbruvybseyrugvi"`)
still correctly reaches `400 M_UNSUPPORTED_ROOM_VERSION` via `RoomActor::create`'s existing
`room_version::rules_for` gate -- worth recording *why* this function itself cannot make that
distinction: `ruma::RoomVersionId::try_from(&str)` never fails for a syntactically valid opaque
token (any 1-32-codepoint string becomes a real, if unknown, `RoomVersionId::_Custom` value per
that type's own doc comment), so "unsupported" is only detectable once `rules_for` is consulted.
Also added: `preset` must be one of the three spec-defined values (`private_chat`/`public_chat`/
`trusted_private_chat`) or `M_BAD_JSON`, rather than silently falling back to `private_chat`'s
defaults for a typo; `visibility` must be `"public"`/`"private"`; `creation_content`, if present,
must be a JSON object, rather than being silently coerced to `{}` by `RoomActor::create_room`'s own
`if !creation_content.is_object() { creation_content = json!({}) }` guard (that guard is now
dead-unreachable from this route, since the route itself rejects a non-object first, but it stays
as `RoomActor::create_room`'s own defensive invariant for any other future caller).

**Proof.** `crates/hs-room/tests/scenario.rs::create_room_validates_request_shape`. Against real
Complement (`apidoc_room_create_test.go::TestRoomCreate`): 13 of 14 subtests pass (numeric and
unknown room versions both now correctly `400`, plus everything that was already passing); the one
remaining failure (`Rooms can be created with an initial invite list (SYN-205)`) is a `/sync`
invite-delivery flake in `hs-user` (track 05) unrelated to `createRoom` itself -- the room is
created and the invite is sent successfully; the test's `MustSyncUntil` on the invitee times out
waiting for `/sync` to report it. Not this track's code.

### 4. `POST /rooms/{roomId}/forget` validates membership and actually forgets

Previously a total no-op (accepted anything, remembered nothing). Now:
`RoomActor::forget(user)` rejects with a new `RoomError::StillJoined` (`400 M_UNKNOWN`, matching
the spec's one documented error shape for this endpoint exactly --
`refs/matrix-spec/data/api/client-server/leaving.yaml`'s example error body, Apache-2.0) if the
user's current membership is `join`; `crate::routes::membership::post_forget` reuses the same
variant/message shape for a room that does not exist at all (the spec does not distinguish the two
cases in its documented response). Otherwise it records the user in a new in-memory `forgotten:
HashSet<OwnedUserId>` field on `RoomActor` (same in-memory-only, does-not-survive-eviction scope
caveat as `txn_dedup`, recorded there and here) and returns success. `RoomActor::can_read_room`
(item 1) refuses `.../messages` outright to a forgotten user even though `shared` visibility would
otherwise permit it (`apidoc_room_forget_test.go`'s "Forgotten room messages cannot be paginated" --
a deliberate, spec-documented exception: forgetting means "stop remembering about a particular
room", not merely "you happen to have left"). **Rejoining clears the forgotten flag**
(`RoomActor::membership_action` removes the target from `forgotten` on a successful `Join`), so
"forget, then get re-invited and rejoin" (the spec test of the same name) is not a permanent exile.

**Proof.** `crates/hs-room/tests/scenario.rs::forget_validates_membership_and_blocks_messages_until_rejoin`.
Against real Complement (`apidoc_room_forget_test.go::TestRoomForget`): 6 of 7 subtests pass ("Can't
forget room you're still in", "Forgotten room messages cannot be paginated", "Can forget room
you've been kicked from", "Can re-join room if re-invited", "Can forget room we weren't an actual
member", "Leave for forgotten room shows up in v2 incremental /sync"). The one remaining failure
("Forgetting room does not show up in v2 initial /sync") needs `hs-user`'s `/sync` to know a room
was forgotten at all -- this crate's `forgotten` set is private, in-process, per-`RoomActor` state
with no query surface for another crate yet; see "Interfaces needed" for the seam track 05 would
need.

### 5. Extensible `m.topic` (MSC3765)

`RoomActor::create_room`: when the top-level `topic` request field is given (not when a
`m.room.topic` arrives only through `initial_state`, which is passed through byte-for-byte,
matching `TestRoomCreate`'s "makes a room with a topic via initial_state" -- no `m.topic` key
expected there), the resulting event's content now also carries
`"m.topic": {"m.text": [{"body": <topic>}]}` alongside the plain `topic` field. `mimetype` is left
unset (defaults to `text/plain` per the schema; the Complement test accepts either). Proof: real
Complement, `TestRoomCreate/.../makes a room with a topic and writes rich topic representation` and
its "...via initial_state overwritten by topic" sibling both pass.

### Complement: before and after, this track's targeted subset

Reproduced with (per `docs/status/14-test-and-conformance.md`'s documented harness):

```bash
./tests/complement/build.sh complement-hs-reimplement:dev
cd refs/complement && COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev \
  go test -v -timeout 10m ./tests/csapi/... \
  -run '^(TestFetchEvent|TestFetchHistoricalJoinedEventDenied|TestFetchHistoricalSharedEvent|TestFetchHistoricalInvitedEventFromBetweenInvite|TestFetchHistoricalInvitedEventFromBeforeInvite|TestFetchEventNonWorldReadable|TestFetchEventWorldReadable|TestRoomCreate|TestPublicRooms|TestRoomForget|TestLeftRoomFixture|TestFetchMessagesFromNonExistentRoom|TestSendAndFetchMessage|TestRoomMessagesLazyLoading|TestRoomMessagesLazyLoadingLocalUser|TestSendMessageWithTxn)$'
```

| Run | Top-level (`func Test*`) | Leaf-level (every `--- PASS/FAIL` line, any depth) |
|---|---|---|
| First this session (items 1/3/4/5 landed, item 2 had a route collision not yet resolved) | 7 pass / 9 fail | 26 pass / 27 fail |
| Final this session (item 2's collision resolved, departed-reader `/state`+`/members` fix added) | 8 pass / 8 fail | **39 pass / 14 fail** |

The remaining 8 top-level failures, all confirmed **not** this track's code (see each item's "Proof"
above for the specific subtest-level breakdown):

- `TestRoomCreate`, `TestRoomForget`: one subtest each, both `/sync` invite/forget-visibility
  timing in `hs-user` (track 05).
- `TestFetchHistoricalInvitedEventFromBetweenInvite`, `TestFetchHistoricalInvitedEventFromBeforeInvite`:
  time out in `MustSyncUntil` waiting for an invite to appear in `/sync`, before the test ever
  reaches a history-visibility assertion -- same `/sync` territory.
- `TestLeftRoomFixture`, `TestSendAndFetchMessage`, `TestRoomMessagesLazyLoading`,
  `TestRoomMessagesLazyLoadingLocalUser`: all fail on `GET .../messages?from=hsu1_...` with `400
  M_INVALID_PARAM: invalid pagination token` -- `hs-user`'s `/sync` issues tokens prefixed `hsu1_`
  in its own format, and `crate::timeline::PaginationToken::from_str` (this crate's room-local
  pagination token, unchanged by this session) does not understand that format. This is a real,
  pre-existing cross-track token-format incompatibility, not a regression from anything in this
  session -- `PaginationToken` parsing was not touched. Recorded under "What is left" and
  "Interfaces needed" below for whichever track picks it up (likely track 05 and 04 jointly, since
  it is exactly the "seam" `docs/workstreams/README.md` asks tracks to write down rather than
  silently patch around).

Full logs: `/tmp/complement-track04-after.log` (first run), `/tmp/complement-track04-after2.log`
(final run) -- not committed (scratch files outside tracked paths).

### Also verified this session

```
cargo fmt -p hs-room
cargo clippy -p hs-room --all-targets -- -D warnings
cargo test -p hs-room                         # 48 tests: 38 lib/unit + 10 scenario, all green
cargo build -p hs-cli --bin hs
cargo test -p hs-loadgen --test real_client -- --nocapture   # still all 17 steps green
```

### What is left

- **The `hsu1_...` pagination-token format mismatch** between `hs-user`'s `/sync` and this crate's
  `GET .../messages` (see above) -- not fixed this session (would need either this crate's
  `PaginationToken` to accept/translate a sync token, or `hs-user` to hand out room-local tokens
  for this purpose; a real cross-track design question, not a quick patch).
- **`hs-user`'s `/sync` does not know about this crate's `forgotten` set** (item 4's one remaining
  Complement failure) or its **real publish flag** (item 2's `Tables::public_rooms`, which
  `hs-user`'s own `public_directory_entry` still does not consult, relying on `join_rule ==
  "public"` instead -- true today only because this session's `/publicRooms` is the one actually
  mounted, per item 2's collision writeup). Both need a query surface from this crate that does not
  exist yet; see "Interfaces needed".
- **`GET .../context`'s own `state` field** still reads the room's *live* current state, not state
  pinned to the target event -- the same category of bug item 1 fixed everywhere else, just not
  reached this session. `RoomActor::state_at_event(event_id)` already computes exactly what this
  field wants; wiring it in is a small, isolated follow-up.
- Everything already listed as not-yet-started in earlier sessions (retention, upgrades, spaces,
  `/hierarchy`, room version 12 create-time two-phase construction refinements, etc.) is unchanged
  by this session.

### Interfaces needed

- **05 (sync)**: a way for `hs-user` to learn (a) whether a user has forgotten a room
  (`RoomActor`'s `forgotten` set, currently private/in-memory/no query surface) and (b) this
  crate's real directory-publish flag (`RoomRegistry::is_directory_public`/
  `list_published_room_ids`, already `pub`, just not yet consulted by `hub.rs`'s
  `public_directory_entry`) -- see "What is left" above for both. Also: `hs-user`'s `/sync` token
  format (`hsu1_...`) and this crate's room-local `PaginationToken` need to either agree on one
  format or have an explicit translation at the boundary; right now a token minted by one is simply
  rejected by the other, breaking `GET .../messages?from=<a /sync token>` for every real client
  (the "Getting messages going forward is limited for a departed room" Complement pattern uses
  exactly this).
- Everything already on this list from earlier sessions (02's `StateKeyId` interning gap, 03's
  cluster ownership routing, 06's soft-fail/backfill, 10's push-evaluation-inputs shape, 14's
  differential-testing coverage) is unchanged by this session.

### Decisions made this session

- **`event_visible_to`/`can_read_room` deny by `404`/`403` respectively, not a shared shape.**
  `.../event`/`.../context` (single-event reads) use `404` uniformly regardless of *why* an event
  is invisible (never-a-member vs. history-visibility-denied vs. does-not-exist), matching the
  spec's own algorithm making no such distinction. `.../messages` (a whole-room read) uses a
  coarser `403` gate (`can_read_room`) for "may not read this room at all", separate from per-event
  filtering for "may not read this specific event" -- because a Complement test
  (`TestFetchMessagesFromNonExistentRoom`) explicitly wants `403` for a nonexistent room on this
  endpoint specifically, unlike `.../event`.
- **`GET .../state`/`.../members`'s deny case returns an empty result (`200`), not `403`.** No
  Complement test in this session's scope stresses "a total stranger calls `GET /state` on a
  non-world-readable room", so an empty, non-leaking response was chosen over inventing an
  unverified error shape. Revisit if a real client or test demonstrates the wrong choice.
- **The room directory's publish flag lives in a new `Tables::public_rooms` keyspace, not as room
  state.** Publication is a server-local administrative fact (like an alias), not something other
  members or servers need to see in the room's own event graph -- see item 2's writeup.
- **`visibility` and `preset` are independent `POST /createRoom` fields**, per the spec: `preset`
  sets join-rule/history-visibility/guest-access defaults; `visibility` only controls directory
  publication. No "contradictory combination" is actually rejected (a `private_chat` room can be
  published, and that is correct, not a conflict) -- the task brief's phrase "contradictory
  preset/visibility combinations" turned out, on reading the spec text precisely
  (`refs/matrix-spec/data/api/client-server/create_room.yaml`), not to name a real spec-defined
  error case; validating each field's own allowed values (done) was the actual, checkable gap.
- **This crate's `GET`/`POST /publicRooms` is the one mounted in the real server, not `hs-user`'s.**
  Not a unilateral decision -- track 05 independently reached the same conclusion from their side
  (see item 2). Recorded here so a future session does not "fix" the collision a second time by
  re-mounting `hs-user`'s copy.
- **`RoomActor::forgotten` stays in-memory-only, not persisted**, same reasoning as `txn_dedup`
  (session 1): the only Complement-visible consequence of losing it on eviction is a forgotten room
  looking un-forgotten again after this process's `RoomRegistry` evicts and reloads it, which is a
  narrower, less user-visible failure mode than the feature not existing at all, and Phase 0 scope
  does not ask for cross-restart durability here yet.

## Session 3 (2026-09-18): `RoomActor::accept_remote_event` -- persisting an event this server did not create

**What this closes.** `docs/status/06-federation.md`'s "the one gap this session could not
close": track 06 landed real inbound federation (`PUT /_matrix/federation/v1/send/{txnId}`
verifying content hashes and signatures, and `make_join`/`send_join`), but every one of those
paths bottomed out at a wall -- `RoomActor` could only build-and-sign *new*, locally-originated
events (`send_event`/`send_event_citing`). There was no entry point that took an already-signed
foreign `hs_model::Event` and stored it as-is. This session adds that entry point. Scope for this
session was **`crates/hs-room/**` and this status file only** -- `hs-federation`, `hs-cli`,
`hs-admin`, `hs-auth`, `hs-user` and `hs-loadgen` were off limits (other tracks in flight); wiring
this into `hs_federation::inbound::RoomWriteSink` is the integration lead's or track 06's next
step, not done here.

### The entry point

```rust
// crates/hs-room/src/actor.rs

pub enum RemoteEventOutcome {
    AlreadyKnown,
    Stored(EventSn),
}

impl<B: KvBackend> RoomActor<B> {
    pub fn accept_remote_event(&mut self, event: Event) -> Result<RemoteEventOutcome, RoomError>;
}

// Async, serialized wrapper -- the one to actually call from another task/crate:
impl<B: KvBackend> RoomActorHandle<B> {
    pub async fn accept_remote_event(&self, event: Event) -> Result<RemoteEventOutcome, RoomError>;
}
```

**What the caller must already have done.** Content hash and signature verification are the
*caller's* job, not this method's. The intended caller is `hs_federation::inbound::verify_pdu`
(which already produces a parsed, hash- and signature-checked `hs_model::Event`), reached through
`hs_federation::inbound::RoomWriteSink::accept_verified_event(room_id: &str, event_id: &str,
event_json: &Value)`. That trait hands back JSON, not an `Event` -- the caller (whatever `hs-cli`
type implements `RoomWriteSink` around a `RoomActorHandle`) needs to re-parse `event_json` via
`hs_model::Event::parse(event_json, room_version)` (cheap: it is the same canonical bytes
`verify_pdu` already validated) before calling `RoomActorHandle::accept_remote_event`. This method
trusts that `event` already passed hash/signature verification and does not redo either check.

**What it authorizes -- exactly two of the spec's three snapshots.** The server-server spec's
"Checks performed on receipt of a PDU" runs event authorization against three different state
snapshots. This method implements the first two, both as **hard** rejections, and does not
implement the third:

1. **Implemented -- the state implied by the event's own `auth_events`.** Builds a
   `hs_state::state_fetch::FlatState` directly from the bodies of the events `event.auth_events`
   names (exactly those events, not a state-store resolution) and runs
   `hs_state::auth::check_event_auth` against it, after `check_auth_events_selection` confirms the
   selection itself is what the spec's auth-events-selection algorithm would have produced against
   this room's actual current state.
2. **Implemented -- the state before the event.** Resolves this room's state at `event`'s own
   `prev_events` (via `hs_state::api::StateStore::current_state`, the same resolution
   `RoomActor::send_event_citing` already authorizes newly-built events against) and runs
   `check_event_auth` against that.
3. **Not implemented -- the room's current state at receipt time.** The spec treats a failure here
   as a **soft failure** (the event is still stored, just excluded from some views and from
   forward-extremity consideration), which needs a persisted-but-excluded event representation
   this crate does not have yet (`hs_model::event::EventFlags` does have an `is_rejected` bit
   already, but nothing reads a "stored but rejected" event back out). Do not read this method as
   implementing soft-fail: every rejection it produces is a hard one.

**What each outcome/error means for the caller:**

- `Ok(RemoteEventOutcome::AlreadyKnown)` -- this actor already held an event with this ID; nothing
  was re-authorized or re-persisted. Maps cleanly onto `RoomWriteSink`'s own
  `WriteOutcome::AlreadyKnown`.
- `Ok(RemoteEventOutcome::Stored(EventSn))` -- newly authorized and durably persisted, reached the
  timeline and the publish stream. Maps onto `WriteOutcome::Stored`.
- `Err(RoomError::MissingAncestors(Vec<OwnedEventId>))` -- `event`'s `prev_events` or
  `auth_events` name an ancestor this actor does not hold. **The ordinary federation case of an
  event arriving before its history has been backfilled, not a protocol violation** -- this method
  never fetches anything itself (no network access from `hs-room`), so the caller (track 06) is
  expected to backfill the named IDs and retry, not to treat this as a rejection. Deliberately a
  distinct variant from `Forbidden` so a `RoomWriteSink` implementation can tell "go backfill and
  retry" apart from "do not retry, this event is bad".
- `Err(RoomError::Forbidden(String))` -- authorization rejected the event against one of the two
  implemented snapshots. **Refused outright: not persisted at all** (see the "hard rejection"
  decision below). The message names which snapshot failed (`"auth-events-implied state rejected
  event: ..."` or `"state-before-the-event rejected event: ..."`).
- `Err(RoomError::State(String))` / `Err(RoomError::Store(_))` -- the state store or the underlying
  KV store failed; not this event's fault.

### Decisions made this session

- **A rejected remote event is refused outright, not stored with a rejected flag.** Both
  authorization failures return `Err` before any KV write happens. The alternative the brief
  offered (store it, marked rejected) would need a code path that reads "stored but rejected"
  events back out (for the third snapshot's soft-fail semantics, for `/state_ids`'
  `auth_chain_ids` including rejected events, etc.) -- nothing in this crate does that yet, so
  storing one would be dead weight with no consumer. Revisit together with soft-fail support.
- **Only two of the three spec snapshots are checked** (auth-events-implied, state-before); the
  third (current-state-at-receipt, soft-fail) is not implemented -- see above. Do not build
  anything downstream that assumes soft-fail exists.
- **`RoomActor::persist` (the existing build-then-persist path's second half) needed no changes at
  all.** Reading it closely: it already takes a plain `Event` and writes it byte-identically
  (`event.canonical_bytes()` into `PersistedEvent.json`, `event.event_id()` for indexing,
  `decode_event_ids(event.json().get("prev_events"))` for extremity bookkeeping) -- it never
  assumed the event was locally built. `accept_remote_event` reuses it unchanged as the "mechanical
  half" the brief asked for; the only new code is the idempotency check, ancestor-presence check,
  and the two authorization calls in front of it.
- **Missing `auth_events`, not just missing `prev_events`, is folded into the same
  `RoomError::MissingAncestors`.** Both indicate the same thing from this actor's point of view
  ("I don't have some ancestor this event cites, backfill me"); a caller does not need to
  distinguish them to decide what to do next (fetch the missing IDs and retry).
- **`extract_redacts`** (a small free function in `actor.rs`) reads `content.redacts` (room
  versions 1-2) or the top-level `redacts` field (room versions 3+) to populate
  `IncomingEvent::redacts`, needed only by the pre-v3 `m.room.redaction` special-case check. This
  duplicates a few lines of logic `crate::pipeline` does not currently expose as a standalone
  function; not worth a shared helper for three lines, noted here in case track 02 adds one.

### Tests (`crates/hs-room/src/actor.rs`, `mod tests`)

Four new tests, all passing, plus every pre-existing test still green (32 lib + 4 scenario tests,
`cargo test -p hs-room`):

- `accept_remote_event_round_trips_byte_identically_and_reaches_publish_stream` -- builds a
  `m.room.message` signed with a signing key and server name **distinct from this actor's own
  identity** (proving `accept_remote_event` neither re-signs nor re-authors it), for a sender
  (`@bob:remote.example`) who joined the room through the ordinary membership pipeline first.
  Asserts: `Stored(_)`; the stored event's `canonical_bytes()` are byte-identical to the
  pre-storage bytes; it appears at the head of `paginate(Backward)`; a subscriber to
  `RoomActor::subscribe()` (the same broadcast stream `hs-user`'s feeds consume) receives a
  `RoomUpdate` naming this event.
- `accept_remote_event_replay_is_a_no_op` -- accepts the same built event twice; asserts the
  second call returns `AlreadyKnown` and the timeline length is unchanged (no duplicate).
- `accept_remote_event_rejects_an_unauthorized_sender_and_it_never_becomes_visible` -- builds the
  same shape of event for a sender (`@eve:remote.example`) who never joined; asserts `Err(Forbidden(_))`,
  and that the event is absent from both `event_by_id` and the timeline. **This is also this
  session's mutation test**: both `check_event_auth` calls in `accept_remote_event` were replaced
  with no-ops (`let _ = &auth_flat; let _ = &state_before;`), this test was confirmed to fail
  (`Stored(EventSn(7))` instead of the expected rejection), and the real checks were then restored
  -- `cargo test -p hs-room` is green again with them back. Full before/after transcript is in this
  session's tool history; not reproduced here, but the mutation and revert both happened.
- `accept_remote_event_with_unknown_prev_events_is_a_distinct_error` -- an event citing a
  `prev_events` ID this actor never persisted; asserts `Err(MissingAncestors(_))` specifically (not
  `Forbidden`, not a panic).

All four tests build their PDUs by hand (canonicalize, hash, sign, `Event::parse`) rather than
through `crate::pipeline::build_and_authorize`, deliberately: that function *builds and authorizes
in one step*, so it cannot produce an event that later fails authorization -- exactly the shape
needed to prove `accept_remote_event`'s own authorization actually runs.

### Verify

```
cargo fmt -p hs-room -- --check
cargo clippy -p hs-room --all-targets -- -D warnings
cargo test -p hs-room
cargo build -p hs-cli --bin hs && cargo test -p hs-loadgen --test real_client -- --nocapture
```

All green as of this session: `hs-room` 32 lib tests (was 28; added the four above) + 4 scenario
tests; `hs-loadgen`'s real-client regression net still passes all 17 steps unchanged (this session
added no client-server-visible behavior, only an internal entry point nothing yet calls).

### What is next (for track 06 / the integration lead)

1. In `hs-cli` (off limits this session): implement `hs_federation::inbound::RoomWriteSink` for a
   type wrapping `hs_room::registry::RoomRegistry`/`RoomActorHandle`:
   `accept_verified_event(room_id, event_id, event_json)` should look up (or load) the room's
   `RoomActorHandle`, `hs_model::Event::parse(event_json, room_version)`, and call
   `handle.accept_remote_event(event).await`, mapping `RemoteEventOutcome`/`RoomError` onto
   `WriteOutcome`/`WriteRejected` (a `RoomError::MissingAncestors` should probably become a
   distinct `WriteRejected` message telling the caller which IDs to backfill, or a new
   `RoomWriteSink` outcome variant if track 06 wants a first-class one -- that trait is track 06's
   to extend).
2. Backfill (`/backfill`, `/get_missing_events`) is still track 06's job entirely; this session
   only makes `hs-room` refuse cleanly (not panic, not silently drop) when it is missing.
3. Soft-fail (the third auth snapshot) and rejected-but-stored events, if/when something downstream
   needs them (e.g. `/state_ids`'s `auth_chain_ids` wanting to include rejected events, or a client
   wanting to see an event that soft-failed for it specifically but not for others).
4. `send_join`'s persistence step (`crates/hs-federation/src/join.rs`, per that crate's own module
   docs) is a second, obvious caller of this same entry point once wired -- it already calls
   `RoomActor::send_event_citing` for events *it* builds; the remote room's existing history it
   receives via `send_join`'s response should go through `accept_remote_event` instead of being
   silently accepted as already-trusted.

## Session 2 (2026-09-18): user profiles

A real `matrix-rust-sdk` client (`cargo test -p hs-loadgen --test real_client`) had exactly one
soft-failing step: no route in the whole workspace mounted `/_matrix/client/v3/profile/{userId}/...`.
This session closed that gap. Scope for this session was **`crates/hs-auth/**`,
`crates/hs-room/**` and this status file only** -- `hs-cli`, `hs-federation` and `hs-admin` were
off limits (other tracks in flight), and none needed editing to make this real end to end (see
"Why no `hs-cli` change was needed" below).

### What is real

- **Storage** (`crates/hs-auth/src/store/mod.rs`): `UserRecord` gained `display_name: Option<String>`
  and `avatar_url: Option<String>` (both `None` by default). `UserStore` gained
  `set_profile_display_name`/`set_profile_avatar_url`. **Naming decision**: deliberately not
  `set_display_name`, because `DeviceStore::set_display_name` already exists (renames a device,
  not a profile) -- the `profile_` prefix on the `UserStore` methods makes the two unambiguous at
  any call site (`user_store.set_profile_display_name(...)` cannot be mistaken for
  `device_store.set_display_name(...)`, and vice versa). Implemented in both
  `store/memory.rs::InMemoryAuthStore` and `store/tables.rs::TablesAuthStore` (the latter via the
  existing `update_user` read-modify-write helper, no new keyspace). Covered in
  `store/shared_tests.rs::profile_fields_round_trip` and
  `set_profile_fields_on_missing_user_is_not_found`, run against both backends by the existing
  `run_all` harness.
- **`GET /api/v1/users` no longer reports `display_name: null` for everyone**:
  `crates/hs-auth/src/admin_directory.rs::to_admin_user` now fills `AdminUser::display_name`/
  `avatar_url` from the same `UserRecord` fields (test:
  `admin_directory::tests::get_user_reflects_profile_fields`).
- **Routes**, in `crates/hs-auth/src/routes/profile.rs`, added to the existing
  `hs_auth::routes::router()` fragment (see "Why hs-auth, not hs-room" below):
  - `GET /profile/{userId}` -- combined document, unauthenticated.
  - `GET /profile/{userId}/displayname`, `PUT /profile/{userId}/displayname`.
  - `GET /profile/{userId}/avatar_url`, `PUT /profile/{userId}/avatar_url`.
  - Spec rules, each with a dedicated test in `routes/profile.rs`'s `tests` module: `PUT` for
    another user's id is `403 M_FORBIDDEN` (`put_displayname_for_another_user_is_forbidden`,
    `put_avatar_url_for_another_user_is_forbidden`); `GET` for an unknown user is
    `404 M_NOT_FOUND` (`get_profile_unknown_user_is_not_found`); `GET` takes no `Requester` at all
    (`get_displayname_unauthenticated_still_works_after_put`), `PUT` does; an unset field is
    omitted from the JSON body, never sent as `null` (`get_profile_omits_unset_fields`,
    asserts the body is exactly `{}`).
- **Propagation** (`crates/hs-room/src/routes/membership.rs`): `extra()`, the function that builds
  the non-`membership` fields of a new `m.room.member` event's content, now looks up the target's
  profile via `RoomState::auth.store` (an `hs_auth::store::UserStore`, already reachable --
  `hs-room` already depended on `hs-auth`) and merges `displayname`/`avatar_url` in for
  `Action::Join`, `Action::Invite` and `Action::Knock` -- the three actions where the event's own
  target is the person whose profile it is. A missing profile field is omitted, matching the
  routes' own rule. Proved with a new end-to-end scenario test,
  `crates/hs-room/tests/scenario.rs::profile_propagates_into_join_invite_and_knock_membership_content`,
  which runs both crates' real routers merged together (the existing `app()` helper already did
  this) and checks: join carries the joiner's `displayname`/`avatar_url`; a user with no profile
  gets neither field; invite carries the invitee's profile; a profile change does **not**
  retroactively edit an already-sent membership event; re-sending join (allowed by
  `crate::membership::TRANSITIONS` as a no-op resend from `PriorState::Join`) picks up the new
  profile.
- **What propagation explicitly does not do** (per the brief's instruction to say so plainly):
  changing a profile does **not** rewrite that user's `m.room.member` event in every room they are
  already joined to (Synapse's `ProfileHandler._update_join_states` behavior). The only way to get
  an updated profile into an existing join's membership event is for the client to send that join
  action again, which the transition table already allowed as a harmless resend before this
  session and required no new code. No automatic per-room fan-out runs on
  `PUT /profile/.../displayname` or `avatar_url`. This is a deliberate scope narrowing, not an
  oversight -- revisit if a client is found that expects live profile propagation without
  resending its own join.
- **Real-client proof**: `cargo build -p hs-cli --bin hs && cargo test -p hs-loadgen --test
  real_client -- --nocapture`. Step 8's line, previously `KNOWN BUG: PUT/GET
  /_matrix/client/v3/profile/.../displayname return 404`, now reads:

  ```
  - alice's display name round-tripped through GET/PUT /profile
  ```

  All 17 scenario steps pass; full run:

  ```
  hs-loadgen scenario completed 17 steps:
    - registered @loadgen-alice:hs-loadgen.test
    - registered @loadgen-bob:hs-loadgen.test
    - logged in @loadgen-alice:hs-loadgen.test on a second device via POST /login
    - alice created room !pYFUmjKftNeDDimdRK:hs-loadgen.test
    - alice invited @loadgen-bob:hs-loadgen.test
    - @loadgen-bob:hs-loadgen.test joined !pYFUmjKftNeDDimdRK:hs-loadgen.test
    - both clients completed a baseline /sync
    - alice sent $aXgp0NZZnQn2JETYI5I1dJg_uS2nR8lHZCqCMRicnOo ("hello bob, this is alice")
    - bob sent $oReZJ-UGUniQmnmIjH6S_F2jvAFQR4YPtqqsBzvpWYQ ("hi alice, bob here")
    - bob's incremental /sync saw alice's message
    - alice's incremental /sync saw bob's message
    - alice's display name round-tripped through GET/PUT /profile
    - room name and topic changes appeared in /sync's timeline
    - room membership lists both @loadgen-alice:hs-loadgen.test and @loadgen-bob:hs-loadgen.test
    - backward /messages page contains alice's message (10 events)
    - both clients logged out
    - post-logout /sync was correctly rejected: ... M_UNKNOWN_TOKEN ...
  test matrix_rust_sdk_talks_to_a_real_hs_serve ... ok
  ```

### Why `hs-auth`, not `hs-room`

Profile data (`UserRecord::display_name`/`avatar_url`) is account data keyed by user id, not room
state -- the same shape as `/account/whoami` and the device endpoints `hs-auth` already owns, and
reading or writing it needs no room context. `hs-room` (which already depends on `hs-auth`) reads
it back out through `AuthState::store` when building new membership content, the same way it
already reaches into `hs-auth` for the `Requester` extractor. Keeping the storage and its HTTP
surface in one crate avoided a second crate needing write access to `hs-auth`'s store internals.

### Why no `hs-cli` change was needed, and what is stale because of that

`hs-auth`'s router (`crates/hs-auth/src/routes/mod.rs::router()`) returns a plain
`axum::Router<AuthState>`, **not** an `hs_http::router::Builder` pair with a `RouteManifest` --
that was already true before this session (see that module's own doc comment: manifest generation
for this crate's routes is hand-mirrored elsewhere). `hs-cli`'s `serve.rs` (`build_router`) already
calls `hs_auth::routes::router().with_state(auth.clone())` and merges that *exact* `axum::Router`
value under both `/_matrix/client/v3` and `/_matrix/client/r0` via
`Builder::merge_router`. Adding the five new `.route(...)` calls to that same `router()` function
(which this session's edit to `crates/hs-auth/src/routes/mod.rs` did) means they are mounted and
served live by any binary that calls `hs_auth::routes::router()` -- including `hs serve` -- with
**no `hs-cli` edit required**. That is exactly how the real-client proof above worked without
touching an off-limits crate.

The cost: `merge_router`'s *manifest* argument for this fragment is
`crate::auth_manifest::routes()` in `hs-cli` (a hand-mirrored `Vec<Route>`, off limits this
session), which this session's five new routes are **not** in. `routes.json` (and anything that
diffs against it) will under-report `hs-auth`'s surface until that list is updated. **For the
integration lead or track 07/14**: add these five entries to `crates/hs-cli/src/auth_manifest.rs`'s
`routes()`, matching the existing entries' shape (`Surface::MatrixClient`, `operation_id` a
reasonable spec-style name, `rate_limited: false` matching the rest of that file's account/device
entries):

| method | path | auth | suggested operation_id |
|---|---|---|---|
| GET | `/profile/{userId}` | `AuthKind::None` | `getUserProfile` |
| GET | `/profile/{userId}/displayname` | `AuthKind::None` | `getDisplayName` |
| PUT | `/profile/{userId}/displayname` | `AuthKind::Matrix` | `setDisplayName` |
| GET | `/profile/{userId}/avatar_url` | `AuthKind::None` | `getAvatarUrl` |
| PUT | `/profile/{userId}/avatar_url` | `AuthKind::Matrix` | `setAvatarUrl` |

### Verify

```
cargo fmt -p hs-room -p hs-auth
cargo clippy -p hs-room -p hs-auth --all-targets -- -D warnings
cargo test -p hs-room -p hs-auth
cargo build -p hs-cli --bin hs && cargo test -p hs-loadgen --test real_client -- --nocapture
```

All green as of this session: `hs-auth` 165 tests (was ~150; added `profile.rs`'s 9,
`shared_tests`'s 2, `admin_directory`'s 1), `hs-room` 28 unit/property tests + 4 scenario tests
(was 3; added `profile_propagates_into_join_invite_and_knock_membership_content`).

### Decisions made this session (in addition to the ones below, carried from session 1)

- `UserStore::set_profile_display_name`/`set_profile_avatar_url` naming, to avoid collision with
  `DeviceStore::set_display_name` -- see "What is real" above.
- Profile routes live in `hs-auth`, not `hs-room` -- see "Why `hs-auth`, not `hs-room`" above.
- Propagation is snapshot-at-send-time only, with no automatic rewrite of already-joined rooms on
  a profile change -- see "What propagation explicitly does not do" above.
- `hs-auth`'s `router()` was extended in place rather than adding a second, `Builder`-based
  fragment, specifically so no `hs-cli` edit was needed this session -- see "Why no `hs-cli`
  change was needed" above. This does leave `routes.json` stale for these five routes until
  `hs-cli/src/auth_manifest.rs` is updated (table above).

## Session 1 (2026-09-18): first pass on a previously untouched crate

## Done

Delivered in the order the brief asked for, all behind `cargo test -p hs-room` (24 tests: 21 unit,
3 scenario) and `cargo clippy -p hs-room --all-targets -- -D warnings` clean.

- **The room actor protocol as a design document and rustdoc** (deliverable 1):
  `docs/design/04-room-actor-protocol.md` plus the authoritative rustdoc on
  `crates/hs-room/src/protocol.rs` (command set, the `RoomUpdate` publish stream field-by-field
  rationale), `crates/hs-room/src/actor.rs` and `crates/hs-room/src/registry.rs` (the hot-state
  cache and its eviction policy, `RoomRegistry::evict_idle`/`spawn_eviction_sweeper`).
- **Event creation and persistence pipeline** (deliverable 2): `crates/hs-room/src/pipeline.rs`
  (build for the room version, fill `prev_events`/`auth_events` from forward extremities and
  current state, size limits via `hs_model::event::Event::parse`, hash and sign, authorize against
  current state via `hs_state::auth`) and `crates/hs-room/src/actor.rs::RoomActor::persist` (one
  `hs_kv::transact` writing the event record, timeline entry, forward-extremity update and relation
  index entry together). Forward extremities, the room-local timeline (`room_pos: i64`, positive
  increasing; the type supports negative backfilled positions, though nothing populates them yet —
  see "Blockers"), and event storage are in `crates/hs-room/src/persist.rs`.
- **The membership state machine** (deliverable 3): `crates/hs-room/src/membership.rs` — `Action`
  (join/invite/leave/kick/ban/unban/knock), the explicit `TRANSITIONS` table, `precheck`, and
  `content_for`. Property-tested per room version in
  `crate::actor::tests::private_room_membership_invariant_holds_across_room_versions` (8 room
  versions) and in `crate::membership::tests` (the table itself). Restricted and knock-restricted
  join rules and third-party invites are enforced by `hs_state::auth` (track 02's crate), which this
  layer calls into rather than re-implementing — see `crates/hs-room/src/pipeline.rs`'s module docs.
- **Client endpoints** (deliverable 4), mounted as an axum router fragment the way `hs-auth` and
  `hs-media` do (`hs_room::routes::router::<B>() -> (Router<RoomState<B>>, RouteManifest)`,
  spec-relative paths): `createRoom` (presets `private_chat`/`public_chat`/`trusted_private_chat`,
  `initial_state`, `power_level_content_override`, `creation_content`, `invite`, `room_alias_name`);
  join/leave/forget/invite/kick/ban/unban/knock (by room ID and by ID-or-alias); send and state
  (with and without a state key); get event, context, members, joined_members, messages (room-local
  pagination tokens, `crate::timeline::PaginationToken`); redact; relations (plain, by `rel_type`,
  by `rel_type` and event type); room aliases (`GET .../aliases`,
  `PUT`/`GET`/`DELETE /directory/room/{roomAlias}`). `crates/hs-room/src/state.rs` follows
  `hs-media`'s `RoomState`/`FromRef<AuthState>`/wrapped-`Requester` pattern for composing with
  `hs-auth`.
- **Tests** (deliverable 5): property tests for the membership machine (per action/prior-state,
  and per room version through the real pipeline); `crates/hs-room/tests/scenario.rs` — three
  `hs-testkit` `Scenario` tests over real HTTP against a router that mounts `hs-auth`'s real router
  and this crate's fragment together, sharing one `AuthState` (create a room, send messages,
  paginate forward and backward with `limit`/`from`, fetch `/event` and `/context`, redact, leave,
  invite-then-join, ban-then-unban-then-rejoin).
- Relations and bundled aggregations: `crates/hs-room/src/relations.rs` (`m.replace`,
  `m.annotation`, `m.thread` bundling) plus the relation index maintained in `RoomActor::persist`;
  not yet wired into `unsigned.m.relations` on `crate::routes::render::client_event_json` (see
  "Next").
- `RoomActor::load`: reconstructs a room actor by replaying its full persisted timeline (the
  `PLAN.md` section 5.3 "everything is derivable from the store" property, exercised by
  `crate::actor::tests::reload_from_the_store_reproduces_the_same_state`), which is what
  `RoomRegistry`'s eviction-then-reload story actually runs.

## In progress

Nothing left mid-implementation; everything above is complete for the scope it claims.

## Next

- Wire `crate::relations::bundle` into `crate::routes::render::client_event_json`'s `unsigned`
  (bundled aggregations are implemented and tested but not yet attached to served events).
- Transaction-ID deduplication on `PUT .../send/...` and `PUT .../redact/...` (replaying the same
  `txnId` should return the same `event_id`, not send a second event) — noted as not implemented at
  each call site; needs a small per-device `(txn_id -> event_id)` table.
- `crate::retention` (purge, `m.room.retention`), room upgrades/tombstones, spaces hierarchy and
  room summaries, `/hierarchy`, `/room_summary`, `/timestamp_to_event`, delayed events (MSC4140),
  sticky events (MSC4354), per-room receipts, room stats, large-room fan-out-on-read threshold —
  all explicitly Phase 1/2 in the brief; not started. `crate::retention`'s module doc records the
  intended shape for purge specifically.
- Room version 12 (hash-based room IDs, MSC4291): rejected outright with a clear error; see
  `docs/rfcs/0010-room-actor-state-store-seam.md` section 3 for exactly what closing it needs.
- `hs_state::StateStore`/state resolution is not wired into `RoomActor` (it uses its own flat
  current-state map instead, correct only for a single writer with no forks); see
  `docs/rfcs/0010-room-actor-state-store-seam.md` section 2. This is also where negative
  (backfilled) timeline positions and a populated backward-extremity table become real — both are
  represented in the types today (`i64` positions, `Tables::extremities_bwd`) but nothing writes
  them yet, since nothing in this pass backfills.
- `Command::PersistInbound` (the federation-facing seam named in
  `docs/design/04-room-actor-protocol.md`) is not implemented; this is deliberate per this track's
  instructions ("Do not implement federation... leave a clear seam"). Track 06 is the consumer.
  **Closed in session 3**: see `RoomActor::accept_remote_event`/`RoomActorHandle::accept_remote_event`
  at the top of this file.
- The hot-state cache holds a room's *entire* event history in memory for the actor's lifetime
  (Phase 0 scope decision, documented on `RoomActor`'s `events` field); a bounded recent-timeline
  window with KV fallback for older events is the natural next step once memory pressure on large
  rooms matters.
- `/joinedRooms`, `/upgrade` and the public room directory (`/publicRooms`) are not implemented
  (the last is out of scope for this track's brief entirely — no room-directory listing state was
  designed).

## Blockers

None. Everything this pass needed from other tracks (`hs-kv`, `hs-tables`, `hs-model`, `hs-state`,
`hs-auth`, `hs-http`, `hs-testkit`) was already landed and usable.

## Interfaces provided

- **Session 8 (RFC 0015), most relevant to track 06 and `hs-cli`**:
  `RoomRegistry::bootstrap_from_remote_join(room_id, room_version, state, auth_chain, join_event)`,
  `RoomActor::{create_from_remote_join, accept_remote_join_with_state}` and
  `RoomActorHandle::accept_remote_join_with_state` -- a room this server's own user joined on a
  resident server, built here from that server's verified `send_join` response. See session 8
  at the top of this file for the contract, the trust decision and the two new keyspaces.
- **Session 5 additions, most relevant to track 05**: `RoomActor::transaction_id_for(event_id,
  viewer, viewer_device) -> Option<&str>` (call through `RoomActorHandle::query` to render
  `unsigned.transaction_id` on `/sync` timeline events the way this crate's own routes now do --
  see session 5, item 6); `RoomActor::thread_roots(requester, participated_only) -> Vec<&Event>`
  (backs `/threads`); `CreateRoomRequest::room_id: Option<OwnedRoomId>` (additive, `None`
  everywhere except `/upgrade`). **Track 05, read session 5's item 7 above**: this crate has no
  public way to answer "has `user` forgotten this room" today (`RoomActor::forget`'s effect is
  entirely private) -- that is very likely why a forgotten room does not leave `/sync`'s `left`
  section; a small additive accessor is the recommended fix, not yet added.
- `hs_room::actor::{RoomActor, RoomActorHandle, CreateRoomRequest, InitialStateEvent}`: the room
  actor and its async handle.
- `hs_room::actor::{RoomActor, RoomActorHandle}::accept_remote_event` /
  `hs_room::actor::RemoteEventOutcome` (session 3): the `Command::PersistInbound` seam -- persists
  an already-verified foreign `hs_model::Event` byte-identically. See this file's session 3 section
  for the exact contract (what the caller must have verified, what each outcome/error means).
  **Track 06**: this is the entry point `RoomWriteSink::accept_verified_event` should call through
  from `hs-cli`.
- `hs_room::protocol::RoomUpdate` (plus `ChangedStateKey`, `MembershipDelta`): the publish stream
  named in `docs/workstreams/README.md`'s week-8 seam. `RoomActorHandle::subscribe()` returns a
  `tokio::sync::broadcast::Receiver<RoomUpdate>`. **Tracks 05, 06, 10, 11**: this is frozen enough
  to build against today; `push_evaluation_inputs` is a placeholder (`Vec<()>`, always empty) until
  track 10 specifies the shape it actually needs — flag if a different field shape than
  "empty until specified" would have been more useful to start against.
- `hs_room::registry::RoomRegistry<B>`: `get_or_load`, `create_room`, `resolve_alias`,
  `evict_idle`, `spawn_eviction_sweeper`, and (session 4) `set_directory_visibility`/
  `is_directory_public`/`list_published_room_ids` -- the room directory's real publish flag. **Track
  05**: `hub.rs`'s `public_directory_entry` should consult `is_directory_public` (or OR it with the
  existing `join_rule == "public"` check) instead of relying on `join_rule` alone; see this file's
  session 4 section, item 2.
- `hs_room::actor::RoomActor`'s history-visibility read-side (session 4):
  `event_visible_to(event, requester)`, `can_read_room(requester)`,
  `full_state_for_reader`/`state_event_for_reader`/`members_for_reader(requester)`, and the pure
  rule table in `hs_room::history_visibility::{HistoryVisibility, base_rule_allows}`. Every read
  route in this crate goes through these now; see session 4's item 1 for the exact contract.
- `hs_room::routes::router::<B>()`: the client-server HTTP fragment, `(Router<RoomState<B>>,
  RouteManifest)`, spec-relative paths, following `hs_auth::routes::router`'s and
  `hs_media::router::authenticated_router`'s convention.
- `hs_room::state::{RoomState<B>, RoomRequester}`, `hs_room::identity::HomeserverIdentity`: the
  axum shared state and the `hs-auth` composition bridge, following `hs-media`'s
  `MediaState`/`MediaRequester` pattern exactly (see `crates/hs-room/src/state.rs`'s module doc for
  why that pattern, not a generic-over-state `hs-auth`, is what exists to compose against).
- `hs_room::membership::{Action, PriorState, TRANSITIONS, precheck, content_for}`: usable by any
  other track needing the membership transition table without going through HTTP.
- `hs_room::error::RoomError` and its `to_matrix_error()`/`IntoResponse` mapping onto
  `hs_http::error::MatrixError` — the frozen error type this track's instructions named.

## Interfaces needed

- **06 (federation) / `hs-cli`, session 8**: call `RoomRegistry::bootstrap_from_remote_join` with
  `hs_federation::outbound_join::RemoteJoinOutcome`'s fields after a successful `join_room`; and
  backfill, so a bootstrapped room has history before the join. Details in session 8's
  "Interfaces needed".
- **02 (state and model)**: `docs/rfcs/0010-room-actor-state-store-seam.md` section 2 — a way to
  intern/look up a `StateKeyId` for a known `(event_type, state_key)` pair independent of already
  holding an event that set it (`StateStore::get` alone cannot answer this today). Track 04 worked
  around it for the single-writer, no-fork case (see the RFC); track 06 will need the real answer
  for inbound federation events that fork state.
- **03 (cluster)**: not consumed yet. `RoomActorHandle`'s mutex-based serialization
  (`crate::protocol`'s "Today's implementation") is a placeholder for whatever `hs-cluster`'s
  ownership routing eventually requires (forwarding a request to the replica that owns a room);
  nothing about `RoomActorHandle`'s public API should need to change for that, but it has not been
  exercised against a real multi-replica scenario.
- **06 (federation)**: `Command::PersistInbound` is now `RoomActorHandle::accept_remote_event`
  (session 3, top of this file) -- implement `hs_federation::inbound::RoomWriteSink` in `hs-cli`
  against it. `docs/design/04-room-actor-protocol.md` section 2 and the RFC's section 2 still apply
  for what federation-driven *forks* need from `hs-state` beyond what session 3's workaround
  provides (session 3 reuses the same single-writer state-view machinery `send_event_citing`
  already used for the fork it can construct; a real multi-server fork arriving in quick
  succession over federation is not yet covered by this crate's own test suite).
- **10 (push)**: `RoomUpdate::push_evaluation_inputs` is an empty placeholder; tell this track the
  concrete shape once designed and it is a small, additive change to `RoomActor::persist`.
- **14 (test/conformance)**: Complement `csapi` room tests and differential tests against Synapse
  are not run against this crate yet — this session's tests are this crate's own unit, property and
  `hs-testkit` scenario tests only.

## Decisions made

- **Session 8**: a remote join supersedes every forward extremity held; outliers are fed to the
  state store with no prev events (their `state_at` is meaningless by design); `room_meta` is
  written with the first *timeline* event only; `load` restores persisted flags. Full list in
  session 8's "Decisions made this session".
- **`hs_state::StateStore`/state resolution is not wired into `RoomActor` in this pass.** The room
  actor maintains its own flat, directly-updated current-state map instead
  (`crate::pipeline::CurrentState`), which is provably correct for a single-writer actor that never
  creates more than one forward extremity (true of every operation this pass implements) and
  sidesteps a real gap in `StateStore`'s current shape (section 2 of the RFC). Recorded in
  `docs/rfcs/0010-room-actor-state-store-seam.md` because it is exactly the kind of thing "tell
  track 02 which methods the room actor calls in which order" (that track's own "Interfaces
  needed") should surface.
- **Room version 12's hash-based room IDs (MSC4291) are not implemented; `RoomActor::create`
  rejects them outright** with a clear `RoomError::UnsupportedRoomVersion` rather than a
  half-correct two-phase construction. `CreateRoomRequest`'s default room version is `"11"`.
  Section 3 of the RFC has the closing plan.
- **The room actor's serialization is a `tokio::sync::Mutex`, not a spawned mailbox task.** Same
  guarantee (one writer at a time), no supervisor/shutdown/backpressure design needed to be correct
  yet, and no external dependency (`hs-cluster`) is wired in that would make the difference
  observable. `crate::protocol`'s module docs have the full rationale; this is an internal
  implementation choice, not a frozen interface decision.
- **Reads (state/event/timeline/members/relations/aliases queries) are not part of the serialized
  command set.** Only mutations (`create_room`, `send_event`, `membership`, `redact`) go through
  `Command`/`Reply`-shaped handle methods that this track's brief calls "the command set"; queries
  are plain async methods reading the same actor under its mutex. Recorded because the brief's
  wording ("the command set: ... query state and timeline") could be read as wanting queries
  serialized too, which would only add latency without adding correctness for a value that is
  already internally consistent once written.
- **The current-state map, event cache and timeline positions are interned by `RoomSn`/`EventSn`
  (via `hs-tables`' shared interning tables) for on-disk keys, but user IDs and event types are
  stored as plain strings inside `hs-room`'s own in-memory maps**, not further interned into
  `hs_model::UserSn`/`TypeId`. `PLAN.md` section 6.1 asks for full interning on every hot-path
  identifier; this pass scoped that down to what `hs-tables`' shared tables already provide
  directly usable, given the size of rooms this pass's tests and scenario exercise. Worth revisiting
  once real-room-size benchmarking (this track's brief's Phase 1/2 performance work) makes the
  string-comparison cost visible.
- **`hs-auth`'s `MatrixError` is converted to `hs-http`'s `MatrixError` at the `RoomRequester`
  extraction boundary** (`crate::state::auth_error_to_matrix_error`), losing the `soft_logout` bit
  (not exposed by `hs-auth`'s `MatrixError` through any public accessor). Recorded as a minor,
  known gap rather than silently dropped.
- **`crate::routes::create_room`, `send_state`, `membership` and `redact` do not implement
  transaction-ID deduplication.** Each call to a `{txnId}`-suffixed endpoint sends a new event even
  if the same `txnId` was already used. Noted at each call site and in "Next".

## Shared dependencies added

Session 8: none (no `Cargo.toml` changed).

None to `[workspace.dependencies]`. `hs-room/Cargo.toml` added ordinary path dependencies on
`hs-model`, `hs-state`, `hs-kv`, `hs-tables`, `hs-auth`, `hs-http` (all already in-tree), and a
dev-dependency feature note: this crate's `[dev-dependencies]` requests `tokio`'s `test-util`
feature explicitly, because `hs-testkit::FakeClock` calls `tokio::time::advance` but
`hs-testkit`'s own `Cargo.toml` does not request that feature — Cargo unifies features per resolved
dependency version across the build graph, so this crate's dev-dependency request also fixes
building `hs-testkit`'s own tests once both are in the same build. Not a change to `hs-testkit`
itself; worth track 14 knowing about in case it surprises a differently-composed build.

## How to verify

```
cargo check -p hs-room
cargo clippy -p hs-room --all-targets -- -D warnings
cargo fmt -p hs-room -- --check
cargo test -p hs-room
```

24 tests: 21 unit/property tests (`cargo test -p hs-room --lib`), 3 `hs-testkit` scenario tests
(`cargo test -p hs-room --test scenario`) driving `hs_room::routes::router` and `hs_auth::routes::router`
together over real HTTP.
