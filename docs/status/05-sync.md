# 05. Sync: status

Track brief: `docs/workstreams/05-sync.md`. Owner crates: `hs-user` (this session's assignment
also covers `crates/hs-loadgen`, the real-client scenario `docs/next-steps.md` item 2 calls "the
single best test of whether this is a homeserver").

Last updated: 2026-09-19 (session: real-client scenario against `hs serve`, run to completion).

## This session's task

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
