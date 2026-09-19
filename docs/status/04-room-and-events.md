# 04 Room and events: status

Track brief: `docs/workstreams/04-room-and-events.md`. Owner crate: `hs-room`.

Last updated: 2026-09-19 (session 5: an urgent cross-track signing bug fixed first
(`docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md`), then `/threads`, `/upgrade`,
idempotent state/join, `unsigned.transaction_id`, and a precise diagnosis of the `/forget`
sync-left regression and of why `/relations`/`/threads` still 404 in a real deployment).

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
