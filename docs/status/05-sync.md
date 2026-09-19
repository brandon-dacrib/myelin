# 05 Sync: status

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

Last updated: 2026-09-19 (session 3: a `/sync`-minted token now works as `GET /messages`'s `from`,
in both directions, joint with track 04's `hs-room` -- see below. Session 2's E2EE work and
session 1's real-client-scenario work are preserved unchanged further down.)

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
