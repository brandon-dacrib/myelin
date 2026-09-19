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

Last updated: 2026-09-19 (session 5: `m.push_rules`/`unread_notifications` consume track 10's
seam, `GET /keys/changes` resolves this crate's own tokens, and `summary` (heroes + member counts)
is no longer hardcoded `{}`. Sessions 1-4 preserved unchanged further down.)

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
