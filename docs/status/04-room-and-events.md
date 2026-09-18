# 04 Room and events: status

Track brief: `docs/workstreams/04-room-and-events.md`. Owner crate: `hs-room`.

Last updated: 2026-09-18 (session 1, first pass on a previously untouched crate).

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
- **06 (federation)**: `Command::PersistInbound` is the named seam; not implemented. See
  `docs/design/04-room-actor-protocol.md` section 2 and the RFC's section 2 for exactly what
  federation-driven forks need from `hs-state` that this pass's workaround does not provide.
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
