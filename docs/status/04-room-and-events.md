# 04 Room and events: status

Track brief: `docs/workstreams/04-room-and-events.md`. Owner crate: `hs-room`.

Last updated: 2026-09-18 (session 3, `RoomActor::accept_remote_event` -- the `Command::PersistInbound`
gap).

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
  `evict_idle`, `spawn_eviction_sweeper`.
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
